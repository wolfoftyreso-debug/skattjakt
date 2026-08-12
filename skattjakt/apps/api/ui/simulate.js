"use strict";

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------
//
// The same shape as the main interface: the page holds no credential. The
// session lives in HttpOnly cookies and the custom header below is what makes
// them safe to accept — a browser will not attach it cross-origin, so a form
// posted from another site cannot act on the session.

const CLIENT_HEADER = "x-skattjakt-client";
let refreshing = null;

function installId() {
  let id = localStorage.getItem("skattjakt_install");
  if (!id) { id = crypto.randomUUID(); localStorage.setItem("skattjakt_install", id); }
  return id;
}

async function authedFetch(url, options = {}) {
  const request = {
    ...options,
    credentials: "same-origin",
    headers: { ...(options.headers || {}), [CLIENT_HEADER]: "web" },
  };
  if (options.body !== undefined) request.headers["content-type"] = "application/json";

  let response = await fetch(url, request);
  if (response.status !== 401) return response;

  // One refresh, serialised. Five concurrent refreshes look like four replayed
  // tokens to the server, and replay detection tears down the whole family.
  if (!refreshing) {
    refreshing = fetch("/v1/auth/refresh", {
      method: "POST",
      credentials: "same-origin",
      headers: { [CLIENT_HEADER]: "web", "content-type": "application/json" },
      body: "{}",
    }).finally(() => { refreshing = null; });
  }
  const refreshed = await refreshing;
  if (!refreshed.ok) { showSignIn(); throw new Error("signed out"); }
  return fetch(url, request);
}

function showSignIn() {
  document.getElementById("signin").classList.remove("hidden");
  document.getElementById("app").classList.add("hidden");
}

async function signIn() {
  const error = document.getElementById("signin-error");
  error.textContent = "";
  const response = await fetch("/v1/auth/sign-in", {
    method: "POST",
    credentials: "same-origin",
    headers: { [CLIENT_HEADER]: "web", "content-type": "application/json" },
    body: JSON.stringify({
      email: document.getElementById("email").value.trim(),
      password: document.getElementById("password").value,
      install_id: installId(),
    }),
  });
  if (!response.ok) {
    const problem = await response.json().catch(() => ({}));
    error.textContent = problem.detail || "Inloggningen misslyckades.";
    return;
  }
  document.getElementById("signin").classList.add("hidden");
  document.getElementById("app").classList.remove("hidden");
  await loadSimulations();
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

const state = {
  catalogue: null,
  simulation: null,   // the loaded model
  result: null,       // the last finished run
  output: null,       // which output is on screen
  runId: null,
  polling: null,
  stale: false,       // inputs edited since the last run
};

const el = (id) => document.getElementById(id);
const escape = (value) =>
  String(value).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

/** Swedish number formatting, and never more precision than a simulation has. */
function num(value, digits) {
  if (value === null || value === undefined || !isFinite(value)) return "—";
  const magnitude = Math.abs(value);
  const decimals = digits !== undefined ? digits
    : magnitude >= 1000 ? 0 : magnitude >= 10 ? 1 : magnitude >= 1 ? 2 : 4;
  return value.toLocaleString("sv-SE", {
    minimumFractionDigits: decimals, maximumFractionDigits: decimals,
  });
}
const percent = (p) =>
  p === null || p === undefined ? "—" : (p * 100).toLocaleString("sv-SE", {
    minimumFractionDigits: 1, maximumFractionDigits: 1 }) + " %";

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

async function loadSimulations() {
  const [catalogue, list] = await Promise.all([
    authedFetch("/v1/simulations/distributions").then((r) => r.json()),
    authedFetch("/v1/simulations").then((r) => r.json()),
  ]);
  state.catalogue = catalogue;
  el("disclaimer").textContent = catalogue.disclaimer;

  const picker = el("simulation");
  picker.innerHTML = "";
  for (const simulation of list.simulations || []) {
    const option = document.createElement("option");
    option.value = simulation.id;
    option.textContent = `${simulation.name} (v${simulation.version})`;
    picker.appendChild(option);
  }
  if (!picker.options.length) {
    el("status").textContent =
      "Ingen simulering finns ännu. Skapa en med POST /v1/simulations.";
    return;
  }
  await loadSimulation(picker.value);
}

async function loadSimulation(id) {
  const response = await authedFetch(`/v1/simulations/${id}`);
  if (!response.ok) { el("status").textContent = "Simuleringen kunde inte läsas."; return; }
  state.simulation = await response.json();
  state.stale = false;
  renderInputs();
  renderRuns();
  el("status").textContent = "";

  // The most recent finished run, so the page opens on a result rather than on
  // an empty panel.
  const latest = (state.simulation.runs || []).find((r) => r.state === "succeeded");
  if (latest) { await loadRun(latest.id); } else { el("output").classList.add("hidden"); el("empty").classList.remove("hidden"); }
}

function renderInputs() {
  const container = el("inputs");
  container.innerHTML = "";
  for (const input of state.simulation.inputs) {
    const group = document.createElement("fieldset");
    const legend = document.createElement("legend");
    legend.textContent = input.name + (input.unit ? ` (${input.unit})` : "");
    group.appendChild(legend);

    const note = document.createElement("p");
    note.className = "muted flush-bottom";
    // The label *and* the guidance. The label alone — "Normalfördelning" —
    // tells nobody when it is the right choice, which is the entire reason the
    // catalogue carries a sentence of guidance per distribution.
    note.textContent = `${input.label}. ${input.guidance}`;
    group.appendChild(note);

    for (const [name, value] of Object.entries(input.parameters)) {
      if (Array.isArray(value)) continue;  // discrete and custom are edited via the API
      const label = document.createElement("label");
      label.textContent = name;
      label.setAttribute("for", `p-${input.id}-${name}`);
      const field = document.createElement("input");
      field.id = `p-${input.id}-${name}`;
      field.type = "number";
      field.step = "any";
      field.value = value;
      field.dataset.input = input.id;
      field.dataset.parameter = name;
      field.addEventListener("input", markStale);
      group.appendChild(label);
      group.appendChild(field);
    }

    const derived = document.createElement("p");
    derived.className = "muted derived";
    derived.textContent = `väntevärde ${num(input.mean)} · standardavvikelse ${num(input.std_dev)}`
      + (input.source ? ` · källa: ${input.source}` : " · källa saknas");
    group.appendChild(derived);
    container.appendChild(group);
  }
}

/** Any edit invalidates what is on screen, and the page must say so. */
function markStale() {
  if (!state.result) return;
  state.stale = true;
  el("stale").classList.remove("hidden");
  el("run").textContent = "Kör om simuleringen";
}

function editedSpec() {
  const spec = JSON.parse(JSON.stringify(state.simulation.spec));
  for (const field of document.querySelectorAll("[data-parameter]")) {
    const input = spec.inputs.find((i) => i.id === field.dataset.input);
    if (!input) continue;
    const value = Number(field.value);
    if (Number.isFinite(value)) input.distribution[field.dataset.parameter] = value;
  }
  return spec;
}

function specChanged() {
  return JSON.stringify(editedSpec()) !== JSON.stringify(state.simulation.spec);
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

async function run(event) {
  event.preventDefault();
  el("run").disabled = true;
  el("status").textContent = "Kör…";

  try {
    // Edited parameters become a new version. The old one keeps working and the
    // old runs keep meaning what they meant.
    if (specChanged()) {
      el("status").textContent = "Sparar ny modellversion…";
      const response = await authedFetch(
        `/v1/simulations/${state.simulation.id}/versions`,
        { method: "POST", body: JSON.stringify({ ...editedSpec(), note: "ändrad i webbgränssnittet" }) });
      if (!response.ok) { return showProblem(response); }
    }

    const seed = el("seed").value.trim();
    const body = { iterations: Number(el("iterations").value) };
    if (seed) body.seed = seed;
    if (el("reason").value.trim()) body.reason = el("reason").value.trim();

    const response = await authedFetch(`/v1/simulations/${state.simulation.id}/run`,
      { method: "POST", body: JSON.stringify(body) });
    if (!response.ok) { return showProblem(response); }

    const payload = await response.json();
    state.runId = payload.run_id;

    if (payload.state === "queued") {
      // Large runs are queued. The page polls, shows progress, and offers to
      // cancel — the run outlives this tab either way.
      el("cancel").classList.remove("hidden");
      el("status").textContent = "Köad. Kör i bakgrunden…";
      await poll();
    } else {
      show(payload);
      el("status").textContent =
        `Klart på ${num(payload.duration_ms, 0)} ms (${num(payload.iterations_per_second, 0)} it/s).`;
    }
    await refreshSimulation();
  } catch (error) {
    el("status").textContent = String(error.message || error);
  } finally {
    el("run").disabled = false;
    el("run").textContent = "Kör simulering";
  }
}

async function poll() {
  clearInterval(state.polling);
  await new Promise((resolve) => {
    state.polling = setInterval(async () => {
      const response = await authedFetch(
        `/v1/simulations/${state.simulation.id}/results?run=${state.runId}`);
      if (!response.ok) return;
      const payload = await response.json();

      if (payload.state === "running" || payload.state === "queued") {
        el("status").textContent =
          `${payload.state === "queued" ? "Köad" : "Kör"} — ${percent(payload.progress)} av ${num(payload.iterations, 0)} iterationer.`;
        return;
      }
      clearInterval(state.polling);
      el("cancel").classList.add("hidden");
      if (payload.state === "succeeded") {
        show(payload);
        el("status").textContent = `Klart på ${num(payload.duration_ms, 0)} ms.`;
      } else if (payload.state === "cancelled") {
        el("status").textContent =
          `Avbruten efter ${num(payload.completed_iterations, 0)} iterationer. Inget resultat sparades.`;
      } else {
        el("status").textContent = payload.error || "Körningen misslyckades.";
      }
      resolve();
    }, 1500);
  });
}

async function cancel() {
  const response = await authedFetch(
    `/v1/simulations/${state.simulation.id}/cancel?run=${state.runId}`, { method: "POST" });
  const payload = await response.json().catch(() => ({}));
  el("status").textContent = payload.detail || "Avbryter…";
}

async function loadRun(runId) {
  const [results, sensitivity, convergence] = await Promise.all([
    authedFetch(`/v1/simulations/${state.simulation.id}/results?run=${runId}`).then((r) => r.json()),
    authedFetch(`/v1/simulations/${state.simulation.id}/sensitivity?run=${runId}`).then((r) => r.json()),
    authedFetch(`/v1/simulations/${state.simulation.id}/convergence?run=${runId}`).then((r) => r.json()),
  ]);
  results.sensitivity = groupBy(sensitivity.sensitivity || [], "output_id");
  results.convergence = groupBy(convergence.convergence || [], "output_id");
  state.runId = runId;
  show(results);
}

function groupBy(rows, key) {
  const out = {};
  for (const row of rows) { (out[row[key]] ||= []).push(row); }
  return out;
}

async function refreshSimulation() {
  const response = await authedFetch(`/v1/simulations/${state.simulation.id}`);
  if (response.ok) { state.simulation = await response.json(); renderRuns(); renderInputs(); }
  state.stale = false;
  el("stale").classList.add("hidden");
}

async function showProblem(response) {
  const problem = await response.json().catch(() => ({}));
  el("status").textContent = problem.detail || `Fel ${response.status}.`;
}

// ---------------------------------------------------------------------------
// Rendering the result
// ---------------------------------------------------------------------------

function show(payload) {
  if (!payload.statistics) { el("status").textContent = "Körningen är inte klar."; return; }
  // An inline run carries its sensitivity and convergence in the response; a
  // queued one has them fetched separately by `loadRun`. Both are flat row
  // arrays in the same shape, so both are grouped here the same way.
  if (Array.isArray(payload.sensitivity)) {
    payload.sensitivity = groupBy(payload.sensitivity, "output_id");
  }
  if (Array.isArray(payload.convergence)) {
    payload.convergence = groupBy(payload.convergence, "output_id");
  }
  state.result = payload;
  state.stale = false;
  el("stale").classList.add("hidden");
  el("empty").classList.add("hidden");
  el("output").classList.remove("hidden");

  const picker = el("output-picker");
  picker.innerHTML = "";
  for (const statistics of payload.statistics) {
    const option = document.createElement("option");
    option.value = statistics.output_id;
    option.textContent = statistics.name || statistics.output_id;
    picker.appendChild(option);
  }
  state.output = picker.value;
  renderOutput();

  el("provenance").textContent =
    `Seed ${payload.seed} · ${num(payload.iterations, 0)} iterationer · motor ${payload.engine_version} · modell ${String(payload.spec_hash).slice(0, 12)}…`;
}

function currentStatistics() {
  return (state.result.statistics || []).find((s) => s.output_id === state.output);
}
function currentShape() {
  return (state.result.shapes || []).find((s) => s.output_id === state.output);
}
function currentOutputSpec() {
  return (state.simulation.outputs || []).find((o) => o.id === state.output) || {};
}

function renderOutput() {
  const statistics = currentStatistics();
  if (!statistics) return;
  const unit = statistics.unit ? ` ${statistics.unit}` : "";

  const figures = [
    ["Median (P50)", num(statistics.p50) + unit, "hälften av utfallen ligger under"],
    ["Medelvärde", num(statistics.mean) + unit, null],
    ["P10", num(statistics.p10) + unit, "pessimistiskt"],
    ["P90", num(statistics.p90) + unit, "optimistiskt"],
    ["Sannolikt intervall", `${num(statistics.p10)} – ${num(statistics.p90)}`, "P10–P90"],
    ["Standardavvikelse", num(statistics.std_dev) + unit, null],
    ["Sämsta utfall", num(statistics.min) + unit, "i denna körning"],
    ["Bästa utfall", num(statistics.max) + unit, "i denna körning"],
  ];
  if (statistics.probability_of_target !== null && statistics.probability_of_target !== undefined) {
    figures.unshift(["Sannolikhet att nå målet", percent(statistics.probability_of_target),
      `mål ${num(currentOutputSpec().target)}${unit}`]);
  }
  if (statistics.probability_below_threshold !== null && statistics.probability_below_threshold !== undefined) {
    figures.push(["Risk under gränsvärdet", percent(statistics.probability_below_threshold),
      `gräns ${num(currentOutputSpec().critical_threshold)}${unit}`]);
  }

  el("figures").innerHTML = figures.map(([label, value, note]) => `
    <div class="figure">
      <dt>${escape(label)}</dt>
      <dd>${escape(value)}${note ? `<br><small>${escape(note)}</small>` : ""}</dd>
    </div>`).join("");

  drawHistogram();
  drawCdf();
  drawSensitivity();
  drawConvergence();
}

// --- histogram -------------------------------------------------------------

const PAD = { left: 46, right: 14, top: 14, bottom: 30 };
const W = 640, H = 260;

function drawHistogram() {
  const shape = currentShape();
  const statistics = currentStatistics();
  const svg = el("histogram");
  if (!shape || !shape.bins.length) { svg.innerHTML = ""; return; }

  const bins = shape.bins;
  const low = bins[0].low, high = bins[bins.length - 1].high;
  const peak = Math.max(...bins.map((b) => b.share), ...(shape.density || [0]));
  const x = (value) => PAD.left + (W - PAD.left - PAD.right) * ((value - low) / (high - low || 1));
  const y = (share) => H - PAD.bottom - (H - PAD.top - PAD.bottom) * (share / (peak || 1));

  let svgText = "";
  bins.forEach((bin, index) => {
    const left = x(bin.low), right = x(bin.high);
    svgText += `<rect class="bar" data-bin="${index}" x="${left.toFixed(1)}" y="${y(bin.share).toFixed(1)}" `
      + `width="${Math.max(right - left - 0.6, 0.6).toFixed(1)}" height="${(H - PAD.bottom - y(bin.share)).toFixed(1)}"></rect>`;
  });

  // The density curve, when it improves readability — which it does not when
  // the output is constant, and the engine returns an empty array for that.
  if (shape.density && shape.density.length === bins.length) {
    const points = shape.density.map((value, index) => {
      const centre = (bins[index].low + bins[index].high) / 2;
      return `${x(centre).toFixed(1)},${y(value).toFixed(1)}`;
    });
    svgText += `<polyline class="density" points="${points.join(" ")}"></polyline>`;
  }

  const markers = [
    [statistics.p10, "P10", ""],
    [statistics.p50, "P50", ""],
    [statistics.p90, "P90", ""],
    [statistics.mean, "medel", ""],
  ];
  const outputSpec = currentOutputSpec();
  if (outputSpec.target !== null && outputSpec.target !== undefined) markers.push([outputSpec.target, "mål", "target"]);
  if (outputSpec.critical_threshold !== null && outputSpec.critical_threshold !== undefined)
    markers.push([outputSpec.critical_threshold, "gräns", "critical"]);

  for (const [value, label, kind] of markers) {
    if (value < low || value > high) continue;
    const px = x(value);
    svgText += `<line class="marker ${kind}" x1="${px.toFixed(1)}" y1="${PAD.top}" x2="${px.toFixed(1)}" y2="${H - PAD.bottom}"></line>`
      + `<text class="marker-label" x="${(px + 3).toFixed(1)}" y="${PAD.top + 9}">${label}</text>`;
  }

  svgText += `<line class="axis" x1="${PAD.left}" y1="${H - PAD.bottom}" x2="${W - PAD.right}" y2="${H - PAD.bottom}"></line>`;
  for (let step = 0; step <= 4; step++) {
    const value = low + (high - low) * (step / 4);
    svgText += `<text x="${x(value).toFixed(1)}" y="${H - PAD.bottom + 14}" text-anchor="middle">${escape(num(value, 0))}</text>`;
  }
  svgText += `<line class="cursor hidden" id="hist-cursor" y1="${PAD.top}" y2="${H - PAD.bottom}"></line>`;

  svg.innerHTML = svgText;
  svg.setAttribute("aria-label",
    `Fördelning för ${currentStatistics().name}. Median ${num(statistics.p50)}, P10 ${num(statistics.p10)}, P90 ${num(statistics.p90)}.`);

  renderHistogramTable(bins);
}

function renderHistogramTable(bins) {
  el("histogram-table").innerHTML =
    `<caption id="histogram-table-caption" class="visually-hidden">Fördelningens intervall och andelar</caption>`
    + "<thead><tr><th>Från</th><th>Till</th><th>Antal</th><th>Andel</th></tr></thead><tbody>"
    + bins.map((bin) => `<tr><td>${escape(num(bin.low))}</td><td>${escape(num(bin.high))}</td>`
        + `<td>${escape(num(bin.count, 0))}</td><td>${escape(percent(bin.share))}</td></tr>`).join("")
    + "</tbody>";
}

/** Hover and keyboard both drive the same readout. */
function trackHistogram(event) {
  const shape = currentShape();
  if (!shape || !shape.bins.length) return;
  const svg = el("histogram");
  const rect = svg.getBoundingClientRect();
  const ratio = (event.clientX - rect.left) / rect.width;
  const px = ratio * W;
  const bins = shape.bins;
  const low = bins[0].low, high = bins[bins.length - 1].high;
  const value = low + (high - low) * ((px - PAD.left) / (W - PAD.left - PAD.right));
  readHistogramAt(value);
}

function readHistogramAt(value) {
  const shape = currentShape();
  const bins = shape.bins;
  const index = bins.findIndex((bin) => value >= bin.low && value <= bin.high);
  if (index < 0) return;
  for (const bar of el("histogram").querySelectorAll(".bar")) bar.classList.remove("hot");
  const bar = el("histogram").querySelector(`[data-bin="${index}"]`);
  if (bar) bar.classList.add("hot");
  const bin = bins[index];
  const cumulative = bins.slice(0, index + 1).reduce((total, b) => total + b.share, 0);
  el("histogram-readout").innerHTML =
    `<strong>${escape(num(bin.low))} – ${escape(num(bin.high))}</strong>: `
    + `${escape(percent(bin.share))} av utfallen (${escape(num(bin.count, 0))} st). `
    + `Kumulativt ${escape(percent(cumulative))}.`;
}

// --- cumulative ------------------------------------------------------------

function drawCdf() {
  const shape = currentShape();
  const svg = el("cdf");
  if (!shape || !shape.cdf.length) { svg.innerHTML = ""; return; }

  const points = shape.cdf;
  const low = points[0].value, high = points[points.length - 1].value;
  const x = (value) => PAD.left + (W - PAD.left - PAD.right) * ((value - low) / (high - low || 1));
  const y = (p) => H - PAD.bottom - (H - PAD.top - PAD.bottom) * p;

  let svgText = `<polyline class="cdfline" points="${
    points.map((point) => `${x(point.value).toFixed(1)},${y(point.probability).toFixed(1)}`).join(" ")}"></polyline>`;

  for (const p of [0.1, 0.5, 0.9]) {
    svgText += `<line class="marker" x1="${PAD.left}" y1="${y(p).toFixed(1)}" x2="${W - PAD.right}" y2="${y(p).toFixed(1)}"></line>`
      + `<text class="marker-label" x="${PAD.left - 4}" y="${(y(p) + 3).toFixed(1)}" text-anchor="end">${p * 100} %</text>`;
  }
  svgText += `<line class="axis" x1="${PAD.left}" y1="${H - PAD.bottom}" x2="${W - PAD.right}" y2="${H - PAD.bottom}"></line>`;
  for (let step = 0; step <= 4; step++) {
    const value = low + (high - low) * (step / 4);
    svgText += `<text x="${x(value).toFixed(1)}" y="${H - PAD.bottom + 14}" text-anchor="middle">${escape(num(value, 0))}</text>`;
  }
  svg.innerHTML = svgText;

  const statistics = currentStatistics();
  svg.setAttribute("aria-label",
    `Kumulativ sannolikhet. 10 % under ${num(statistics.p10)}, 50 % under ${num(statistics.p50)}, 90 % under ${num(statistics.p90)}.`);
  el("cdf-readout").innerHTML =
    `Det är <strong>90 %</strong> sannolikhet att utfallet blir minst <strong>${escape(num(statistics.p10))}</strong>.`;
}

function trackCdf(event) {
  const shape = currentShape();
  if (!shape || !shape.cdf.length) return;
  const rect = el("cdf").getBoundingClientRect();
  const px = ((event.clientX - rect.left) / rect.width) * W;
  const points = shape.cdf;
  const low = points[0].value, high = points[points.length - 1].value;
  const value = low + (high - low) * ((px - PAD.left) / (W - PAD.left - PAD.right));

  let nearest = points[0];
  for (const point of points) {
    if (Math.abs(point.value - value) < Math.abs(nearest.value - value)) nearest = point;
  }
  el("cdf-readout").innerHTML =
    `Det är <strong>${escape(percent(1 - nearest.probability))}</strong> sannolikhet att utfallet blir minst `
    + `<strong>${escape(num(nearest.value))}</strong>, och ${escape(percent(nearest.probability))} att det blir högst så.`;
}

// --- sensitivity -----------------------------------------------------------

function drawSensitivity() {
  const rows = (state.result.sensitivity || {})[state.output] || [];
  const container = el("tornado");
  if (!rows.length) {
    container.innerHTML = "";
    el("sensitivity-note").textContent = "Ingen känslighetsanalys för detta utfall.";
    return;
  }
  const largest = Math.max(...rows.map((row) => row.variance_contribution), 0.0001);
  container.innerHTML = rows.map((row) => `
    <div class="name" title="${escape(row.input_name)}">${escape(row.rank)}. ${escape(row.input_name)}</div>
    <div class="track"><div class="fill"></div></div>
    <div class="share">${escape(percent(row.variance_contribution))}</div>`).join("");
  // The bar widths are set through the CSSOM rather than written into the
  // markup as `style="width:…"`. The policy blocks inline style attributes; a
  // property assignment is not one, and this is the only place where a length
  // genuinely comes from the data.
  container.querySelectorAll(".fill").forEach((fill, index) => {
    fill.style.width = `${(100 * rows[index].variance_contribution / largest).toFixed(1)}%`;
  });

  const unreferenced = rows.filter((row) => !row.referenced).length;
  el("sensitivity-note").textContent =
    `Andel av den förklarade variansen, beräknad på ${num(rows[0].sample_size, 0)} iterationer med rangkorrelation.`
    + (unreferenced ? ` ${unreferenced} indata påverkar inte detta utfall och redovisas utan värde.` : "");
}

// --- convergence -----------------------------------------------------------

function drawConvergence() {
  const rows = (state.result.convergence || {})[state.output] || [];
  const table = el("convergence-table");
  if (!rows.length) { table.innerHTML = ""; return; }

  table.innerHTML = "<thead><tr><th>Iterationer</th><th>Medel</th><th>Median</th><th>P10</th><th>P90</th></tr></thead><tbody>"
    + rows.map((row) => `<tr><td>${escape(num(row.iterations, 0))}</td><td>${escape(num(row.mean))}</td>`
        + `<td>${escape(num(row.median))}</td><td>${escape(num(row.p10))}</td><td>${escape(num(row.p90))}</td></tr>`).join("")
    + "</tbody>";

  const warning = rows.find((row) => row.warning);
  const box = el("convergence-warning");
  if (warning && !rows[0].stable) {
    box.textContent = warning.warning;
    box.classList.remove("hidden");
  } else {
    box.classList.add("hidden");
  }
}

// --- runs ------------------------------------------------------------------

function renderRuns() {
  const runs = state.simulation.runs || [];
  el("runs-table").innerHTML =
    "<thead><tr><th>Körd</th><th>Status</th><th>Iterationer</th><th>Seed</th><th>Modell</th>"
    // Not an empty <th>: a screen reader announces the column and then nothing.
    // Hidden visually because the heading would be noise beside a single button.
    + '<th><span class="visually-hidden">Åtgärd</span></th></tr></thead><tbody>'
    + runs.map((run) => `<tr>
        <td>${escape(new Date(run.requested_at).toLocaleString("sv-SE"))}</td>
        <td>${escape(run.state)}</td>
        <td>${escape(num(run.iterations, 0))}</td>
        <td class="seed">${escape(run.seed)}</td>
        <td>v${escape(run.model_version)}</td>
        <td>${run.state === "succeeded"
              ? `<button type="button" class="secondary" data-run="${escape(run.id)}">Visa</button>`
              : ""}</td>
      </tr>`).join("")
    + "</tbody>";

  for (const button of el("runs-table").querySelectorAll("[data-run]")) {
    button.addEventListener("click", () => loadRun(button.dataset.run));
  }
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

el("signin-button").addEventListener("click", signIn);
el("password").addEventListener("keydown", (e) => { if (e.key === "Enter") signIn(); });
el("model").addEventListener("submit", run);
el("cancel").addEventListener("click", cancel);
el("simulation").addEventListener("change", (e) => loadSimulation(e.target.value));
el("output-picker").addEventListener("change", (e) => { state.output = e.target.value; renderOutput(); });
el("reset").addEventListener("click", () => {
  renderInputs();
  el("seed").value = "";
  el("reason").value = "";
  state.stale = false;
  el("stale").classList.add("hidden");
  el("run").textContent = "Kör simulering";
});
el("iterations").addEventListener("change", markStale);

el("histogram").addEventListener("mousemove", trackHistogram);
el("cdf").addEventListener("mousemove", trackCdf);

// Keyboard equivalents. A chart reachable only with a mouse is a chart half the
// people who need it cannot read.
el("histogram").addEventListener("keydown", (event) => {
  const shape = currentShape();
  if (!shape || !shape.bins.length) return;
  const bars = [...el("histogram").querySelectorAll(".bar")];
  const current = bars.findIndex((bar) => bar.classList.contains("hot"));
  let next = current;
  if (event.key === "ArrowRight") next = Math.min(bars.length - 1, current + 1);
  else if (event.key === "ArrowLeft") next = Math.max(0, current < 0 ? 0 : current - 1);
  else if (event.key === "Home") next = 0;
  else if (event.key === "End") next = bars.length - 1;
  else return;
  event.preventDefault();
  const bin = shape.bins[next];
  readHistogramAt((bin.low + bin.high) / 2);
});

for (const tab of document.querySelectorAll('[role="tab"]')) {
  tab.addEventListener("click", () => {
    for (const other of document.querySelectorAll('[role="tab"]')) {
      const selected = other === tab;
      other.setAttribute("aria-selected", String(selected));
      el(other.getAttribute("aria-controls")).classList.toggle("hidden", !selected);
    }
  });
}

// Whether a session already exists is answered by trying, not by guessing —
// but with a plain fetch rather than `authedFetch`. The latter answers a 401 by
// attempting a refresh, so a first-time visitor with no cookies at all produced
// a 401 and then a 422 in the console before being shown the sign-in form. A
// question that is expected to be answered "no" should not look like a failure.
fetch("/v1/simulations", {
  credentials: "same-origin",
  headers: { [CLIENT_HEADER]: "web" },
})
  .then((response) => {
    if (!response.ok) return showSignIn();
    document.getElementById("app").classList.remove("hidden");
    return loadSimulations();
  })
  .catch(showSignIn);
