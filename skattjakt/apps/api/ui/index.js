const STAGES = [
  ["reading_documents", "Läser bokslutet"],
  ["understanding_structure", "Förstår företagets ekonomiska struktur"],
  ["identifying_areas", "Identifierar relevanta skattemässiga områden"],
  ["searching_opportunities", "Söker efter möjliga möjligheter"],
  ["checking_rules", "Kontrollerar regler och villkor"],
  ["falsifying", "Försöker falsifiera upptäckterna"],
  ["verifying_calculations", "Verifierar beräkningar"],
  ["prioritising", "Prioriterar resultat"],
];

const DEFAULT_DISCLAIMER =
  "Skattjakt är ett analys- och upptäcktsverktyg. Resultaten är preliminära och ska " +
  "inte betraktas som juridisk rådgivning, revisionsuttalande, skattebesked eller " +
  "garanti om skatteåterbäring eller besparing. Identifierade möjligheter bör " +
  "verifieras mot aktuella regler och företagets fullständiga underlag innan någon " +
  "åtgärd vidtas.";

// Not set on load: the markup already carries it, and overwriting it here
// would put the page back to depending on this file having run.

function go(id) {
  document.querySelectorAll(".step").forEach(s => s.classList.remove("active"));
  document.getElementById(id).classList.add("active");
  window.scrollTo({ top: 0 });
}

const el = id => document.getElementById(id);

/* The headers every call carries.
 *
 * There is no token here, deliberately. The session lives in an HttpOnly
 * cookie the browser attaches on its own and this script cannot read — so an
 * XSS on this page cannot steal a credential and use it later from elsewhere.
 *
 * `x-skattjakt-client` is not decoration. The server refuses to honour the
 * cookie without it, which is what stops another site causing an authenticated
 * request: a cross-origin request cannot set a custom header without a CORS
 * preflight this API never grants. */
function authHeaders() {
  return {
    "content-type": "application/json",
    "x-skattjakt-client": "web",
  };
}

async function signIn() {
  const button = el("signin-button");
  const error = el("signin-error");
  error.hidden = true;
  button.disabled = true;
  button.textContent = "Loggar in…";

  try {
    const response = await fetch("/v1/auth/sign-in", {
      method: "POST",
      headers: authHeaders(),
      /* `same-origin` is the default, and stating it is the point: the cookie
       * the server sets must be accepted, and a future change to `omit` would
       * break sign-in in a way that reads like a server fault. */
      credentials: "same-origin",
      body: JSON.stringify({
        email: el("email").value.trim(),
        password: el("password").value,
        install_id: installId(),
        device_name: "Webbläsare",
      }),
    });

    const payload = await response.json().catch(() => ({}));
    if (!response.ok) {
      /* Branch on `code`, never on `detail`: `detail` is prose written for a
       * person and will be translated. */
      error.textContent = payload.code === "account_temporarily_locked"
        ? "För många misslyckade försök. Försök igen om en stund."
        : "Fel e-postadress eller lösenord.";
      error.hidden = false;
      return;
    }

    /* The password is not kept anywhere, not even in the field. */
    el("password").value = "";
    go("company");
  } catch (e) {
    error.textContent = "Kunde inte nå tjänsten.";
    error.hidden = false;
  } finally {
    button.disabled = false;
    button.textContent = "Logga in";
  }
}

/* A stable identifier for this browser installation, so signing in again does
 * not add a second row to the customer's device list. Not a security boundary:
 * it is client-supplied and scoped to the user by the server. */
function installId() {
  let id = localStorage.getItem("skattjakt.install_id");
  if (!id) {
    id = crypto.randomUUID();
    localStorage.setItem("skattjakt.install_id", id);
  }
  return id;
}

/* One refresh at a time.
 *
 * Five concurrent 401s must not become five refreshes: the server rotates on
 * each, so four of them would look like a replayed token and the whole session
 * family would be revoked — signing the customer out for being on a slow
 * connection. */
let refreshInFlight = null;

async function refreshSession() {
  if (!refreshInFlight) {
    refreshInFlight = fetch("/v1/auth/refresh", {
      method: "POST",
      headers: authHeaders(),
      credentials: "same-origin",
    }).finally(() => { refreshInFlight = null; });
  }
  return refreshInFlight;
}

/* A fetch that refreshes once on a 401 and gives up after that.
 *
 * Once, not in a loop: if the refresh itself fails the session is gone, and
 * retrying would spin against a server that has already said no. */
async function authedFetch(path, options = {}) {
  const request = () => fetch(path, {
    ...options,
    headers: { ...authHeaders(), ...(options.headers ?? {}) },
    credentials: "same-origin",
  });

  let response = await request();
  if (response.status === 401) {
    const refreshed = await refreshSession();
    if (refreshed.ok) {
      response = await request();
    } else {
      go("start");
      return response;
    }
  }
  return response;
}

async function signOut() {
  await fetch("/v1/auth/sign-out", {
    method: "POST",
    headers: authHeaders(),
    credentials: "same-origin",
  }).catch(() => {});
  go("start");
}
const tri = id => { const v = el(id).value; return v === "" ? null : v === "true"; };

/** Öre to Swedish kronor, grouped. Money crosses the wire as an integer. */
function kr(ore) {
  return (ore / 100).toLocaleString("sv-SE", {
    style: "currency", currency: "SEK", maximumFractionDigits: 0,
  });
}
function range(low, high) {
  return low === high ? kr(low) : `${kr(low)}–${kr(high)}`;
}
const escape = s => String(s ?? "").replace(/[&<>"]/g, c =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

async function readFile(file) {
  const buffer = await file.arrayBuffer();
  let binary = "";
  const bytes = new Uint8Array(buffer);
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}

function showError(message) {
  const box = el("upload-error");
  box.textContent = message;
  box.classList.remove("hidden");
  el("analyse-btn").disabled = false;
  go("upload");
}

/* ------------------------------------------------------------------ checkout
 *
 * Everything here is built from `/v1/shop`. The prices, what is for sale and
 * the consent wording are the server's, because a price typed into this file is
 * a price that can disagree with the one the customer is charged.
 *
 * The client never decides that a payment happened. It creates an order, shows
 * the Swish token, and then asks the server how it went — and the server asks
 * Swish. */
let shop = null;
let chosenProduct = null;
let currentOrder = null;
let pollTimer = null;

async function loadShop() {
  if (shop) return shop;
  const response = await fetch("/v1/shop", { headers: authHeaders() });
  if (!response.ok) throw new Error(`butiken svarade ${response.status}`);
  shop = await response.json();
  return shop;
}

/* Whether this deployment charges at all. A build with payments off runs the
 * analysis directly, exactly as before — the checkout is not shown, rather than
 * shown and skipped. */
function paymentsRequired() {
  return Boolean(shop && shop.payments_required);
}

function renderProducts() {
  const list = el("products");
  list.innerHTML = "";
  chosenProduct = null;

  for (const product of shop.products) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "product";
    button.setAttribute("aria-pressed", "false");
    button.dataset.product = product.id;

    if (product.available) {
      button.innerHTML =
        `<span class="price">${escape(product.price)}</span>` +
        `<strong>${escape(product.title)}</strong>` +
        `<p class="meta flush">${escape(product.description)}</p>`;
      button.onclick = () => chooseProduct(product.id);
    } else {
      // Listed and closed rather than hidden. A service described on /tjanster
      // and missing here reads as an oversight, not as a decision.
      button.disabled = true;
      button.innerHTML =
        `<strong>${escape(product.title)}</strong>` +
        `<p class="meta flush">Inte öppen för köp ännu.</p>`;
    }
    list.appendChild(button);
  }

  // The common case is one available product; preselecting it saves a tap and
  // cannot surprise anyone, because there is nothing else to pick.
  const available = shop.products.filter(p => p.available);
  if (available.length === 1) chooseProduct(available[0].id);
}

function chooseProduct(id) {
  chosenProduct = id;
  document.querySelectorAll(".product").forEach(b =>
    b.setAttribute("aria-pressed", String(b.dataset.product === id)));
  updateBuyButton();
}

/* Immediate delivery needs the acknowledgement, so the button is disabled until
 * it is given. Disabled rather than refused on click: a customer should see
 * what is missing before they act, not after. */
function updateBuyButton() {
  const immediate = document.querySelector('input[name="delivery"]:checked')?.value === "immediate";
  el("consent-block").classList.toggle("hidden", !immediate);
  el("buy-btn").disabled = !chosenProduct || (immediate && !el("consent").checked);
}

async function openCheckout() {
  try {
    await loadShop();
  } catch (error) {
    el("shop-error").textContent = `Kunde inte läsa priserna: ${error.message}`;
    el("shop-error").classList.remove("hidden");
    return;
  }
  el("cancellation-days").textContent = String(shop.cancellation_period_days);
  el("consent-wording").textContent = shop.consent.wording;
  el("consent-version").textContent = `Ordalydelse version ${shop.consent.version}.`;
  renderProducts();
  updateBuyButton();
  go("checkout");
}

async function buy() {
  const immediate = document.querySelector('input[name="delivery"]:checked').value === "immediate";
  el("buy-btn").disabled = true;
  el("shop-error").classList.add("hidden");

  try {
    const response = await authedFetch("/v1/orders", {
      method: "POST",
      body: JSON.stringify({
        product: chosenProduct,
        delivery: immediate ? "immediate" : "after_cancellation_period",
        accepts_loss_of_cancellation_right: immediate && el("consent").checked,
      }),
    });
    const payload = await response.json();
    if (!response.ok) {
      el("shop-error").textContent =
        `${payload.title ?? "Köpet kunde inte startas"}: ${payload.detail ?? ""}`;
      el("shop-error").classList.remove("hidden");
      return;
    }
    currentOrder = payload;
    showPayment(payload);
  } catch (error) {
    el("shop-error").textContent = `Kunde inte nå tjänsten: ${error.message}`;
    el("shop-error").classList.remove("hidden");
  } finally {
    updateBuyButton();
  }
}

function showPayment(order) {
  el("pay-amount").textContent = `${order.amount} för ${order.product === "control_review"
    ? "Skattjakt Kontroll" : order.product === "private_analysis"
    ? "Privatanalys" : "Bolagsanalys"}.`;
  el("pay-error").classList.add("hidden");
  el("pay-status").textContent = "Väntar på att betalningen bekräftas…";

  // The app switch on a phone, the QR code elsewhere. Both are the same token;
  // neither is evidence that anything was paid.
  const token = order.swish_token;
  const onPhone = /Android|iPhone|iPad/.test(navigator.userAgent);
  if (token && onPhone) {
    el("swish-link").href = `swish://paymentrequest?token=${encodeURIComponent(token)}`;
    el("swish-app").classList.remove("hidden");
    el("swish-qr").classList.add("hidden");
  } else if (token) {
    el("qr").textContent = token;
    el("swish-qr").classList.remove("hidden");
    el("swish-app").classList.add("hidden");
  }

  go("paying");
  pollOrder(order.order_id);
}

/* Asks the server how the payment went, and the server asks Swish.
 *
 * Polling reads a database row rather than calling Swish, so polling faster
 * does not drive traffic at the payment provider. It still stops: a payment
 * nobody completes must not leave a browser asking forever. */
function pollOrder(orderId) {
  clearInterval(pollTimer);
  let attempts = 0;

  pollTimer = setInterval(async () => {
    attempts += 1;
    if (attempts > 150) {           // about five minutes at two seconds
      clearInterval(pollTimer);
      el("pay-status").textContent =
        "Betalningen har inte bekräftats. Du kan stänga sidan — " +
        "har pengarna dragits hör av dig, och analysen körs eller betalas tillbaka.";
      return;
    }

    try {
      const response = await authedFetch(`/v1/orders/${orderId}`);
      if (!response.ok) return;
      const order = await response.json();
      currentOrder = order;

      if (order.state === "paid" || order.state === "consumed") {
        clearInterval(pollTimer);
        // A buyer who kept their right to cancel bought an analysis that starts
        // later. Saying so is the whole point of having offered the choice.
        if (order.keeps_right_to_cancel && new Date(order.deliverable_from) > new Date()) {
          el("pay-status").textContent =
            `Betalt. Analysen startar ${new Date(order.deliverable_from)
              .toLocaleDateString("sv-SE")}, när ångerfristen löpt ut. ` +
            "Fram till dess kan du ångra köpet.";
          return;
        }
        analyse(order.order_id);
      } else if (order.state === "declined" || order.state === "failed") {
        clearInterval(pollTimer);
        el("pay-error").textContent =
          order.note || "Betalningen gick inte igenom. Inget har debiterats.";
        el("pay-error").classList.remove("hidden");
        el("pay-status").textContent = "";
      }
    } catch {
      // A failed poll is a failed question, not a failed payment. The next one
      // asks again.
    }
  }, 2000);
}

/* The paid path: store the document, draw the analysis against the order, wait.
 *
 * Returns the report, or `null` having already shown why it could not be had.
 * The waiting case is not an error — a buyer who kept their right to cancel
 * bought an analysis that starts in a fortnight, and saying so is the point of
 * having offered them the choice. */
async function paidAnalysis(document_, accountsState, orderId) {
  const stored = await authedFetch("/v1/documents", {
    method: "POST",
    body: JSON.stringify({ ...document_, kind: "annual_accounts" }),
  });
  const storedPayload = await stored.json();
  if (!stored.ok) {
    showError(`${storedPayload.title ?? "Underlaget kunde inte sparas"}: ${storedPayload.detail ?? ""}`);
    return null;
  }

  const started = await authedFetch("/v1/analyses/stored", {
    method: "POST",
    body: JSON.stringify({
      document_version_ids: [storedPayload.document_version_id],
      accounts_state: accountsState,
      order_id: orderId,
    }),
  });
  const startedPayload = await started.json();
  if (!started.ok) {
    showError(`${startedPayload.title ?? "Analysen kunde inte startas"}: ${startedPayload.detail ?? ""}`);
    return null;
  }

  // Poll until it finishes. The server reports the stage it is actually on;
  // nothing here invents progress.
  const id = startedPayload.analysis_id;
  for (let attempt = 0; attempt < 300; attempt += 1) {
    await new Promise(resolve => setTimeout(resolve, 2000));
    const status = await authedFetch(`/v1/analyses/${id}`);
    if (!status.ok) continue;
    const state = await status.json();
    if (state.status === "succeeded") {
      const report = await authedFetch(`/v1/analyses/${id}/report`);
      if (report.ok) return report.json();
      showError("Analysen blev klar men rapporten kunde inte hämtas.");
      return null;
    }
    if (state.status === "failed") {
      showError(state.error || "Analysen misslyckades. Köpet återbetalas.");
      return null;
    }
  }
  showError("Analysen tar ovanligt lång tid. Den fortsätter i bakgrunden — ladda om sidan om en stund.");
  return null;
}

/* Runs the analysis.
 *
 * Two paths, and the difference is not cosmetic. Without payments the stateless
 * route takes the documents inline and answers with the finished report. With
 * payments the documents are stored first, the analysis is drawn against the
 * paid order, and the result is polled — because the paid route is asynchronous
 * and the order has to be redeemed inside the transaction that creates the
 * analysis. */
async function analyse(orderId) {
  el("upload-error").classList.add("hidden");
  el("analyse-btn").disabled = true;

  const file = el("file").files[0];
  const pasted = el("paste").value.trim();
  if (!file && !pasted) return showError("Ladda upp en fil eller klistra in siffrorna.");

  let document_ = { filename: "bokslut.txt", mime_type: "text/plain", text: pasted };
  if (file) {
    const isPdf = file.name.toLowerCase().endsWith(".pdf");
    document_ = {
      filename: file.name,
      mime_type: isPdf ? "application/pdf"
        : file.name.toLowerCase().endsWith(".csv") ? "text/csv" : "text/plain",
      content_base64: await readFile(file),
    };
  }

  const body = {
    company: {
      name: el("name").value,
      org_number: el("orgnr").value,
      fiscal_year: { start: el("fy-start").value, end: el("fy-end").value },
      employee_count: Number(el("employees").value) || null,
      owner_count: Number(el("owners").value) || null,
      in_group: tri("in-group"),
      has_vehicles: tri("vehicles"),
      does_development_work: tri("dev"),
      owners_active_in_company: tri("active"),
    },
    documents: [document_],
    accounts_state: el("state").value,
  };

  go("analysis");
  // The stages are shown as the request runs. The server reports the same
  // sequence; this reflects it rather than inventing progress.
  const list = el("stages");
  list.innerHTML = STAGES.map(([, label]) => `<li>${label}</li>`).join("");
  let step = 0;
  const ticker = setInterval(() => {
    if (step < list.children.length) list.children[step++].classList.add("done");
  }, 260);

  try {
    if (orderId) {
      const report = await paidAnalysis(document_, body.accounts_state, orderId);
      clearInterval(ticker);
      if (!report) return;
      render(report);
      return go("result");
    }
    const response = await authedFetch("/v1/analyses", {
      method: "POST",
      body: JSON.stringify(body),
    });
    clearInterval(ticker);
    const payload = await response.json();

    if (!response.ok) {
      return showError(`${payload.title ?? "Något gick fel"}: ${payload.detail ?? ""}`);
    }
    render(payload);
    go("result");
  } catch (error) {
    clearInterval(ticker);
    showError(`Kunde inte nå tjänsten: ${error.message}`);
  } finally {
    el("analyse-btn").disabled = false;
  }
}

// How far each cited authority has been checked. A reader shown "30 kap. 5 §"
// beside a figure reasonably assumes somebody opened it, so the citation says
// whether anybody did rather than leaving the flattering reading available.
const SOURCE_STATES = {
  verified: { label: "kontrollerad", warn: false },
  mismatch: { label: "källan motsäger regeln", warn: true },
  unreachable: { label: "kunde inte hämtas", warn: true },
  unretrieved: { label: "ej kontrollerad", warn: true },
};

// Only http(s), because a citation URL is rendered as a link and a rule set is
// data: `javascript:` in that field would otherwise be a script the CSP never
// sees.
function safeUrl(url) {
  try {
    const parsed = new URL(url, window.location.origin);
    return parsed.protocol === "https:" || parsed.protocol === "http:" ? parsed.href : null;
  } catch {
    return null;
  }
}

function citationList(rule) {
  const citations = rule.citations || [];
  if (!citations.length) return rule.source ? ` — ${escape(rule.source)}` : "";
  return `<ul class="citations">${citations.map(c => {
    const state = SOURCE_STATES[c.state] || SOURCE_STATES.unretrieved;
    const url = safeUrl(c.url || "");
    const reference = url
      ? `<a href="${escape(url)}" target="_blank" rel="noopener noreferrer">${escape(c.reference)}</a>`
      : escape(c.reference);
    const when = c.retrieved_at ? ` ${escape(String(c.retrieved_at).slice(0, 10))}` : "";
    return `<li>${reference} <span class="tag${state.warn ? " warn" : ""}">${
      escape(state.label)}${when}</span></li>`;
  }).join("")}</ul>`;
}

function card(o) {
  const values = (o.evidence || []).filter(e => e.type === "document_value");
  const rules = (o.evidence || []).filter(e => e.type === "rule");
  const assumptions = (o.evidence || []).filter(e => e.type === "assumption");
  const money = o.impact.low === 0 && o.impact.high === 0
    ? "Ingen beräknad effekt"
    : range(o.impact.low, o.impact.high);

  const statusLabels = {
    identified: "Identifierad", investigate: "Undersök",
    verify: "Verifiera", warning: "Varning", rejected: "Avvisad",
  };
  const categoryLabels = {
    tax: "Skatt", costs: "Kostnader", vat: "Moms", personnel: "Personal",
    investments: "Investeringar", research_and_development: "FoU", risk: "Risk",
  };
  const warn = o.status === "warning" || o.status === "verify";

  return `<article class="card">
    <header>
      <h3>${escape(o.title)}</h3>
      <span class="amount">${money}</span>
    </header>
    <p class="meta">
      <span class="tag${warn ? " warn" : ""}">${statusLabels[o.status] ?? o.status}</span>
      · ${escape(categoryLabels[o.category] ?? o.category)} · confidence ${o.confidence.score} %
    </p>
    <p class="gap-m">${escape(o.rationale)}</p>
    <p class="meta"><strong>Nästa steg:</strong> ${escape(o.recommended_action)}</p>
    ${values.length || rules.length ? `<details>
      <summary>Varför säger Skattjakt detta?</summary>
      <ul>
        ${values.map(v => `<li>${escape(v.kind)}: <strong>${kr(v.value)}</strong>${
          v.page ? ` — sida ${v.page}` : ""}${
          v.excerpt ? `<br><code>${escape(v.excerpt)}</code>` : ""}</li>`).join("")}
        ${rules.map(r => `<li>${escape(r.title)}${citationList(r)}</li>`).join("")}
        ${assumptions.map(a => `<li>Antagande: ${escape(a.statement)}</li>`).join("")}
      </ul>
    </details>` : ""}
    ${(o.missing_information || []).length ? `<details>
      <summary>Vad saknas (${o.missing_information.length})</summary>
      <ul>${o.missing_information.map(m => `<li>${escape(m)}</li>`).join("")}</ul>
    </details>` : ""}
  </article>`;
}

function render(result) {
  const s = result.summary;
  el("disclaimer").textContent = result.disclaimer || DEFAULT_DISCLAIMER;

  el("headline").textContent = s.found_nothing
    ? "Skattjakten hittade inga tydliga möjligheter på det underlag vi fått. " +
      "Det betyder inte att det inte finns möjligheter — det betyder att vi inte har " +
      "tillräckligt stark evidens för att flagga dem."
    : `Vi hittade ${s.identified_opportunities} ${s.identified_opportunities === 1 ? "sak" : "saker"} som kan vara värda att undersöka.`;

  el("counts").innerHTML = [
    [s.high_priority_count, "hög prioritet"],
    [s.needs_investigation_count, "bör undersökas"],
    [s.missing_information_count, "kräver mer underlag"],
    [s.warnings_count, "varningar"],
  ].map(([n, label]) => `<div><strong>${n}</strong>${label}</div>`).join("");

  el("total").textContent = range(s.estimated_total.low, s.estimated_total.high);
  el("total-note").textContent = s.estimated_total.high === 0
    ? "Ingen beräknad ekonomisk effekt på det underlag som lämnats."
    : "Ett intervall, inte ett besked. Verifiera innan du agerar.";

  const high = result.opportunities.filter(o => o.priority.band === "high");
  el("start-here").innerHTML = high.length
    ? high.map(card).join("")
    : `<p class="meta">Inget fynd har nått hög prioritet på det här underlaget.</p>`;

  el("opportunities").innerHTML = result.opportunities.length
    ? result.opportunities.map(card).join("")
    : `<p class="meta">Inga fynd.</p>`;

  const warnings = result.warnings || [];
  el("warnings-section").classList.toggle("hidden", warnings.length === 0);
  el("warnings").innerHTML = warnings.map(w =>
    `<div class="card"><p class="flush">${escape(w.message)}</p>${
      w.detail ? `<p class="meta">${escape(w.detail)}</p>` : ""}</div>`).join("");

  const missing = result.missing_information || [];
  el("missing-section").classList.toggle("hidden", missing.length === 0);
  el("missing").innerHTML = missing.map(m =>
    `<div class="card"><p class="flush">${escape(m.description)}</p>
     <p class="meta">${escape(m.unlocks)}</p></div>`).join("");

  el("covered").innerHTML = (result.covered_areas || []).map(a =>
    `<div class="meta">${escape(a.category)} — ${a.rules_evaluated} regler prövade, ${a.findings} fynd</div>`
  ).join("");

  el("next-steps").innerHTML = (result.recommended_actions || []).length
    ? `<ol>${result.recommended_actions.map(a => `<li>${escape(a)}</li>`).join("")}</ol>`
    : `<p class="meta">Inga åtgärder föreslås på det här underlaget.</p>`;

  el("limitations").innerHTML = (result.limitations || [])
    .map(l => `<div class="meta">${escape(l.statement)}</div>`).join("");
}


/* ------------------------------------------------------------- wiring it up
 *
 * Every handler is attached here rather than written as `onclick="..."` in the
 * markup, and that is not a style preference.
 *
 * The Content-Security-Policy these pages are served under is
 * `script-src 'self'` with no `unsafe-inline`, which blocks inline event
 * handlers exactly as it blocks inline <script>. The page carried nine of them,
 * so in a real browser its buttons did nothing at all: sign in, continue, start
 * the analysis, go back — every one refused with
 *
 *     Refused to execute inline event handler because it violates the following
 *     Content Security Policy directive: "script-src 'self'"
 *
 * The tests did not catch it because they audited this page statically and only
 * clicked on the simulation page, which had never used inline handlers.
 * `tests/e2e/interface.mjs` now clicks through this one and fails on any
 * console violation, so a handler written back into the markup breaks the build
 * rather than the product. */

/** Where the "Starta Skattjakten" button goes.
 *
 * Straight into the analysis when this deployment does not charge, and into the
 * checkout when it does. Asked of the server rather than assumed, because a
 * client that assumed wrong would either show a checkout nobody needs or run an
 * analysis nobody paid for. */
async function startAnalysis() {
  try {
    await loadShop();
  } catch {
    // The shop is unreachable. Fall through to the free path: it will refuse
    // with the server's own reason if payment is in fact required, which is a
    // better answer than a checkout built from nothing.
    return analyse();
  }
  return paymentsRequired() ? openCheckout() : analyse();
}

document.addEventListener("DOMContentLoaded", () => {
  el("signin-button")?.addEventListener("click", signIn);
  el("analyse-btn")?.addEventListener("click", startAnalysis);
  el("buy-btn")?.addEventListener("click", buy);

  // Plain navigation between steps, declared on the element it belongs to.
  document.querySelectorAll("[data-go]").forEach(button =>
    button.addEventListener("click", () => go(button.dataset.go)));

  // The consent gate: the buy button stays disabled until an immediate delivery
  // has been acknowledged.
  document.querySelectorAll('input[name="delivery"]').forEach(radio =>
    radio.addEventListener("change", updateBuyButton));
  el("consent")?.addEventListener("change", updateBuyButton);
});
