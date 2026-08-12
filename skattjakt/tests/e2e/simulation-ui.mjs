// The simulation interface, driven in a real browser against the real API.
//
// The point of this suite is narrow and it is the one thing no other test can
// establish: that the page is wired to the backend rather than to a fixture.
// Section 24 asks that no mock implementation is left behind a screen and that
// every control is connected to real functionality — which is a claim about the
// browser, so it has to be checked in one.
//
// It asserts on what a person would see: that the figures on the page are the
// figures the API returned, that the histogram is drawn from them, that editing
// an input tells the user the result is stale, and that pressing the button
// again produces a new run against a new model version.
//
// Usage: node tests/e2e/simulation-ui.mjs http://127.0.0.1:PORT EMAIL PASSWORD
// Requires: playwright, and a running API with a simulation already created.

// Resolved through a path rather than a bare specifier: ESM ignores NODE_PATH,
// and Playwright is installed outside this repository — it is a test tool, not
// a dependency of the product, and adding a package.json here to satisfy the
// resolver would make it look like one.
// Playwright ships CommonJS, so a dynamic import wraps it in `default`.
const playwrightModule = process.env.PLAYWRIGHT_MODULE || "playwright";
const imported = await import(playwrightModule);
const { chromium } = imported.default ?? imported;

const [base, email, password] = process.argv.slice(2);
if (!base || !email || !password) {
  console.error("usage: simulation-ui.mjs <base-url> <email> <password>");
  process.exit(2);
}

let passed = 0;
let failed = 0;
const ok = (what) => { console.log(`  ok    ${what}`); passed++; };
const bad = (what, detail) => { console.log(`  FAIL  ${what}${detail ? ` (${detail})` : ""}`); failed++; };
const check = (what, condition, detail) => (condition ? ok(what) : bad(what, detail));

const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_PATH || undefined,
  args: ["--no-sandbox"],
});
const context = await browser.newContext({ locale: "sv-SE" });
const page = await context.newPage();

// Anything the page logs as an error is a defect: this interface loads nothing
// from anywhere else, so there is no third-party noise to filter out.
const consoleErrors = [];
page.on("pageerror", (error) => consoleErrors.push(String(error)));
page.on("console", (message) => {
  if (message.type() === "error") consoleErrors.push(message.text());
});

// Every request the page makes, so "is it talking to the real API" is answered
// by observation rather than by reading the source.
const apiCalls = [];
page.on("response", (response) => {
  const url = new URL(response.url());
  if (url.pathname.startsWith("/v1/")) {
    apiCalls.push({ path: url.pathname, status: response.status() });
  }
});

try {
  // --- signing in --------------------------------------------------------
  await page.goto(`${base}/simulations`, { waitUntil: "networkidle" });
  check("the page loads", await page.title() === "Skattjakt — simulering");

  await page.waitForSelector("#signin:not(.hidden)", { timeout: 10_000 });
  ok("an unauthenticated visitor is shown the sign-in form and no data");

  await page.fill("#email", email);
  await page.fill("#password", password);
  await page.click("#signin-button");
  await page.waitForSelector("#app:not(.hidden)", { timeout: 15_000 });
  ok("signing in reveals the application");

  // One console error is expected and is not a defect: the page asks the server
  // whether a session exists, because an HttpOnly cookie cannot be read from
  // script, and for an anonymous first visit the honest answer is 401. Anything
  // after this point is a real failure, so the list is cleared here rather than
  // filtered later — a filter would also hide a second, genuine 401.
  check("the anonymous session probe is the only thing that failed so far",
    consoleErrors.length <= 1 && consoleErrors.every((e) => e.includes("401")),
    consoleErrors.join(" | "));
  consoleErrors.length = 0;

  // The session is in cookies the page cannot read. Checked here rather than
  // assumed, because it is the property the whole cookie design exists for.
  const cookies = await context.cookies();
  const session = cookies.find((c) => c.name === "skattjakt_session");
  check("the session cookie is HttpOnly", session && session.httpOnly, JSON.stringify(session));
  check("and SameSite=Strict", session && session.sameSite === "Strict");
  const readable = await page.evaluate(() => document.cookie);
  check("and script on the page cannot read it", !readable.includes("skattjakt_session"),
    `document.cookie was ${JSON.stringify(readable)}`);

  // --- the model is loaded from the API ----------------------------------
  await page.waitForSelector("#inputs fieldset", { timeout: 10_000 });
  const inputCount = await page.locator("#inputs fieldset").count();
  check("the inputs are rendered from the stored model", inputCount >= 3, `${inputCount} inputs`);

  const guidance = await page.locator("#inputs fieldset p.muted").first().textContent();
  check("each input carries its distribution's Swedish guidance",
    Boolean(guidance && guidance.length > 20), guidance);

  check("the catalogue was fetched from the server",
    apiCalls.some((c) => c.path === "/v1/simulations/distributions" && c.status === 200));

  // --- running -----------------------------------------------------------
  await page.selectOption("#iterations", "10000");
  await page.fill("#seed", "20260812");
  await page.fill("#reason", "webbtest");
  await page.click("#run");
  await page.waitForSelector("#output:not(.hidden)", { timeout: 60_000 });
  ok("pressing the button runs a simulation and shows a result");

  const runCall = apiCalls.find((c) => c.path.endsWith("/run"));
  check("the result came from a real POST to the API", runCall && runCall.status === 200,
    JSON.stringify(runCall));

  // The decisive check: the numbers on screen are the numbers the API holds.
  const shown = await page.evaluate(() => {
    const figures = {};
    for (const figure of document.querySelectorAll("#figures .figure")) {
      figures[figure.querySelector("dt").textContent.trim()] =
        figure.querySelector("dd").childNodes[0].textContent.trim();
    }
    return figures;
  });
  const simulationId = await page.evaluate(() => document.getElementById("simulation").value);
  const truth = await page.evaluate(async (id) => {
    const response = await fetch(`/v1/simulations/${id}/statistics`, {
      credentials: "same-origin", headers: { "x-skattjakt-client": "web" },
    });
    return response.json();
  }, simulationId);

  const outputId = await page.evaluate(() => document.getElementById("output-picker").value);
  const statistics = truth.statistics.find((s) => s.output_id === outputId);
  const formatted = (value) => {
    const magnitude = Math.abs(value);
    const decimals = magnitude >= 1000 ? 0 : magnitude >= 10 ? 1 : magnitude >= 1 ? 2 : 4;
    return value.toLocaleString("sv-SE", {
      minimumFractionDigits: decimals, maximumFractionDigits: decimals,
    });
  };
  check("the median on screen is the median the API returned",
    shown["Median (P50)"]?.startsWith(formatted(statistics.p50)),
    `screen ${shown["Median (P50)"]} vs api ${formatted(statistics.p50)}`);
  check("and so is P10", shown["P10"]?.startsWith(formatted(statistics.p10)),
    `screen ${shown["P10"]} vs api ${formatted(statistics.p10)}`);
  check("and so is P90", shown["P90"]?.startsWith(formatted(statistics.p90)),
    `screen ${shown["P90"]} vs api ${formatted(statistics.p90)}`);

  // --- the charts --------------------------------------------------------
  const bars = await page.locator("#histogram rect.bar").count();
  check("the histogram is drawn as bars", bars >= 8, `${bars} bars`);
  const density = await page.locator("#histogram polyline.density").count();
  check("with a density curve over it", density === 1);
  const markers = await page.locator("#histogram line.marker").count();
  check("and dashed markers for the percentiles, target and threshold", markers >= 4, `${markers}`);

  const label = await page.getAttribute("#histogram", "aria-label");
  check("the chart carries its headline numbers for a screen reader",
    Boolean(label && label.includes("Median")), label);

  const rows = await page.locator("#histogram-table tbody tr").count();
  check("and the same data is available as a table", rows === bars, `${rows} rows vs ${bars} bars`);

  // Keyboard navigation. A chart reachable only with a mouse is unreadable to
  // anyone who does not use one.
  await page.focus("#histogram");
  await page.keyboard.press("Home");
  await page.keyboard.press("ArrowRight");
  const readout = await page.textContent("#histogram-readout");
  check("the histogram is navigable by keyboard with a readout",
    Boolean(readout && readout.includes("%")), readout);

  await page.click("#tab-cdf");
  await page.waitForSelector("#panel-cdf:not(.hidden)");
  const cdfPoints = await page.getAttribute("#cdf polyline.cdfline", "points");
  check("the cumulative chart is drawn", Boolean(cdfPoints && cdfPoints.split(" ").length > 100),
    `${cdfPoints ? cdfPoints.split(" ").length : 0} points`);
  const cdfReadout = await page.textContent("#cdf-readout");
  check("and reads as a sentence about probability",
    Boolean(cdfReadout && cdfReadout.includes("sannolikhet")), cdfReadout);

  await page.click("#tab-sens");
  await page.waitForSelector("#panel-sens:not(.hidden)");
  const tornadoRows = await page.locator("#tornado .track").count();
  check("the sensitivity ranking is drawn", tornadoRows >= 3, `${tornadoRows} rows`);

  await page.click("#tab-conv");
  await page.waitForSelector("#panel-conv:not(.hidden)");
  const convergenceRows = await page.locator("#convergence-table tbody tr").count();
  check("the convergence series is shown", convergenceRows >= 2, `${convergenceRows} rows`);

  await page.click("#tab-runs");
  await page.waitForSelector("#panel-runs:not(.hidden)");
  const runRows = await page.locator("#runs-table tbody tr").count();
  check("previous runs are listed for comparison", runRows >= 1, `${runRows} rows`);

  const provenance = await page.textContent("#provenance");
  check("the run's seed, engine version and model hash are on the page",
    Boolean(provenance && provenance.includes("20260812") && provenance.includes("Seed")),
    provenance);

  // --- the stale-result rule ---------------------------------------------
  //
  // The failure this whole interface exists to avoid: old numbers sitting
  // beside new inputs with nothing saying so.
  await page.click("#tab-hist");
  const firstMedian = shown["Median (P50)"];
  const field = page.locator("#inputs input[data-parameter]").first();
  await field.fill(String(Number(await field.inputValue()) * 1.5));
  await page.waitForSelector("#stale:not(.hidden)", { timeout: 5_000 });
  ok("editing an input warns that the result on screen is out of date");
  const buttonText = await page.textContent("#run");
  check("and the button asks for a re-run", buttonText.includes("Kör om"), buttonText);

  await page.click("#run");
  await page.waitForFunction(
    (previous) => {
      const dd = document.querySelector("#figures .figure dd");
      return dd && !dd.textContent.startsWith(previous);
    },
    firstMedian,
    { timeout: 60_000 },
  );
  ok("re-running produces a different result from the changed inputs");
  check("the stale warning clears", await page.locator("#stale.hidden").count() === 1);
  check("and the edit was stored as a new model version",
    apiCalls.some((c) => c.path.endsWith("/versions") && c.status === 201));

  // --- risk communication ------------------------------------------------
  const disclaimer = await page.textContent("#disclaimer");
  check("the disclaimer is on the page and comes from the server",
    Boolean(disclaimer && disclaimer.includes("simulering")), disclaimer);

  const body = await page.textContent("body");
  for (const forbidden of ["kommer att", "garanterar", "säkerställer"]) {
    check(`the page never claims "${forbidden}"`, !body.includes(forbidden));
  }

  // --- responsiveness ----------------------------------------------------
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(200);
  const overflow = await page.evaluate(() =>
    document.documentElement.scrollWidth - document.documentElement.clientWidth);
  check("the page does not scroll sideways on a phone", overflow <= 1, `${overflow}px of overflow`);
  const chartVisible = await page.locator("#histogram").isVisible();
  check("and the chart is still visible there", chartVisible);

  check("nothing errored in the browser once signed in", consoleErrors.length === 0,
    consoleErrors.slice(0, 3).join(" | "));
} catch (error) {
  bad("the suite ran to completion", String(error && error.message ? error.message : error));
} finally {
  await browser.close();
}

console.log(`\npassed ${passed}, failed ${failed}`);
process.exit(failed === 0 ? 0 : 1);
