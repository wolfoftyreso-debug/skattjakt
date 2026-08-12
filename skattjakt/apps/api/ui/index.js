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

async function analyse() {
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
        ${rules.map(r => `<li>${escape(r.title)} — ${escape(r.source)}</li>`).join("")}
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
