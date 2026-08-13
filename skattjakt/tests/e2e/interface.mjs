// The main interface, clicked through in a real browser.
//
// Why this exists, and what it caught
// ===================================
//
// The page was written with nine `onclick="..."` attributes. It is served under
// a Content-Security-Policy of `script-src 'self'` with no `unsafe-inline`,
// which blocks inline event handlers exactly as it blocks inline `<script>`.
// So in any real browser every button on the product's main page did nothing:
//
//     Refused to execute inline event handler because it violates the following
//     Content Security Policy directive: "script-src 'self'"
//
// Sign in, continue, start the analysis, go back — all inert, for as long as
// the page had existed. Nothing caught it. The accessibility suite audited this
// page statically, and the only suite that clicked anything drove the
// simulation page, which had never used an inline handler.
//
// The lesson is narrow and worth stating: a page that renders is not a page
// that works, and a security header that is asserted in a unit test is not one
// that has been tried against the markup it protects. This suite clicks.
//
// It fails on any console CSP violation or uncaught page error, so a handler
// written back into the markup breaks the build rather than the product.
//
// Usage: node tests/e2e/interface.mjs http://127.0.0.1:PORT

import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_MODULE);

const base = process.argv[2];
if (!base) {
  console.error("usage: interface.mjs BASE_URL");
  process.exit(2);
}

let passed = 0;
let failed = 0;
const ok = message => { console.log(`  ok    ${message}`); passed += 1; };
const bad = (message, detail) => {
  console.log(`  FAIL  ${message}`);
  if (detail) console.log(detail);
  failed += 1;
};

const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_PATH || undefined,
});
const page = await browser.newPage();

// Everything the browser refused, and everything that threw. Both are failures
// of this suite: a page whose script cannot run is a page that does not work,
// however well it renders.
const refusals = [];
const errors = [];
page.on("console", message => {
  const text = message.text();
  if (/Content Security Policy|Refused to (execute|load|apply)/i.test(text)) {
    refusals.push(text);
  }
});
page.on("pageerror", error => errors.push(String(error)));

/** Which step is on screen. The interface shows exactly one. */
const activeStep = () =>
  page.evaluate(() => document.querySelector(".step.active")?.id ?? null);

try {
  await page.goto(`${base}/`, { waitUntil: "networkidle" });

  // --- the page's own script is allowed to run ---------------------------
  const scriptRan = await page.evaluate(() => typeof window.go === "function");
  scriptRan
    ? ok("the page's script loaded and defined its functions")
    : bad("the page's script did not load", "        window.go is not a function");

  (await activeStep()) === "start"
    ? ok("the first screen is the sign-in step")
    : bad("the first screen is not the sign-in step");

  // --- the buttons actually do something ---------------------------------
  //
  // The whole point. `go('company')` is reached only through a click, so a
  // handler the browser refused to attach shows up here as a step that did not
  // change — which is exactly what a person would experience.
  await page.evaluate(() => window.go("company"));
  await page.click('[data-go="upload"]');
  await page.waitForTimeout(150);
  (await activeStep()) === "upload"
    ? ok("a navigation button moves to the next step")
    : bad("clicking Fortsätt did nothing", "        the step did not change");

  await page.click('[data-go="company"]');
  await page.waitForTimeout(150);
  (await activeStep()) === "company"
    ? ok("and back again")
    : bad("clicking Tillbaka did nothing");

  // Sign in with credentials that cannot work: what is asserted is that the
  // handler ran and the page reported the server's answer, not that anyone got
  // in. An inert button shows nothing at all.
  await page.evaluate(() => window.go("start"));
  await page.fill("#email", "ingen@example.test");
  await page.fill("#password", "fel lösenord som inte finns");
  await page.click("#signin-button");
  await page.waitForSelector("#signin-error:not([hidden])", { timeout: 10_000 })
    .then(() => ok("the sign-in button reaches the API and shows what it said"))
    .catch(() => bad("the sign-in button did nothing", "        no error was shown"));

  // --- the shop the checkout is built from -------------------------------
  const shop = await page.evaluate(async () => {
    const response = await fetch("/v1/shop", { headers: { "x-skattjakt-client": "web" } });
    return response.ok ? response.json() : null;
  });

  if (!shop) {
    bad("the shop endpoint did not answer");
  } else {
    ok("the interface can read what is for sale");

    shop.consent?.wording?.includes("förlorar min ångerrätt")
      ? ok("the consent wording comes from the server")
      : bad("the shop did not carry the consent wording");

    shop.products?.length === 3
      ? ok("all three products are listed, available or not")
      : bad(`expected three products, got ${shop.products?.length}`);

    // The product with no rules behind it is listed and closed. A checkout that
    // offered it would take money for an empty report.
    const priv = shop.products?.find(p => p.id === "private_analysis");
    priv && priv.available === false
      ? ok("a product with no rules behind it is listed as unavailable")
      : bad("the shop offered a product this build cannot deliver");

    const company = shop.products?.find(p => p.id === "company_analysis");
    company?.price === "69,00 kr"
      ? ok("the price comes from the server, not from the page")
      : bad(`the company analysis price was ${company?.price}`);
  }

  // --- the checkout renders from it --------------------------------------
  await page.evaluate(() => window.openCheckout());
  await page.waitForTimeout(400);

  (await activeStep()) === "checkout"
    ? ok("the checkout opens")
    : bad("the checkout did not open");

  const buyDisabled = () => page.evaluate(() => document.getElementById("buy-btn").disabled);
  const productButtons = await page.locator(".product").count();
  productButtons === 3
    ? ok("every product is on the checkout, closed ones included")
    : bad(`expected three product buttons, got ${productButtons}`);

  const unavailableDisabled = await page.evaluate(() =>
    document.querySelector('.product[data-product="private_analysis"]')?.disabled);
  unavailableDisabled
    ? ok("the closed one cannot be selected")
    : bad("a product this build cannot deliver was selectable");

  // Nothing can be bought before something is chosen. Two products are
  // available in this build, so neither is preselected.
  (await buyDisabled())
    ? ok("nothing can be bought before a product is chosen")
    : bad("the buy button was live with no product selected");

  await page.click('.product[data-product="company_analysis"]');
  await page.waitForTimeout(120);
  (await buyDisabled())
    ? bad("choosing a product left the buy button disabled")
    : ok("choosing one makes the purchase available");

  // The consent gate. Choosing immediate delivery must disable the button until
  // the acknowledgement is given — the law asks for an express act, and a
  // customer should see what is missing before they click, not after.
  await page.check('input[name="delivery"][value="immediate"]');
  await page.waitForTimeout(120);
  (await buyDisabled())
    ? ok("immediate delivery cannot be bought without the acknowledgement")
    : bad("the buy button was live with no consent given");

  const consentVisible = await page.locator("#consent-block").isVisible();
  consentVisible
    ? ok("and the words being agreed to are on screen")
    : bad("the consent wording was not shown");

  await page.check("#consent");
  await page.waitForTimeout(120);
  (await buyDisabled())
    ? bad("the buy button stayed disabled after consent was given")
    : ok("ticking it makes the purchase available");

  // And back to the cautious option: the acknowledgement must not survive it.
  await page.check('input[name="delivery"][value="after_cancellation_period"]');
  await page.waitForTimeout(120);
  const hidden = await page.locator("#consent-block").isHidden();
  hidden
    ? ok("choosing to keep the right to cancel hides the acknowledgement")
    : bad("the consent block stayed on screen for a delivery that needs none");

  // --- nothing was refused, nothing threw --------------------------------
  refusals.length === 0
    ? ok("the browser refused nothing on the page")
    : bad(
        `the browser refused ${refusals.length} thing(s)`,
        refusals.slice(0, 3).map(r => `        ${r.slice(0, 150)}`).join("\n"),
      );

  errors.length === 0
    ? ok("and nothing threw")
    : bad(
        `${errors.length} uncaught error(s)`,
        errors.slice(0, 3).map(e => `        ${e.slice(0, 150)}`).join("\n"),
      );
} catch (error) {
  bad("the run reached the end", `        ${error?.message ?? error}`);
} finally {
  await browser.close();
}

console.log(`\npassed ${passed}, failed ${failed}`);
process.exit(failed === 0 ? 0 : 1);
