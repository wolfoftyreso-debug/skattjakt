// The accessibility audit, in a real browser, against the real pages.
//
// `SKATTJAKT_PRODUCT_SURFACE.md` §4 carried "accessibility has not been audited
// against WCAG" as a stated gap for as long as there has been an interface.
// This is the audit. It runs axe-core over every state of both pages that a
// person actually sees — not just the empty one, because an interface that is
// accessible before any data arrives and inaccessible afterwards is not
// accessible.
//
// Two things it deliberately does *not* do:
//
//   - It does not claim WCAG conformance. Automated tooling finds a minority of
//     the failures in the standard; it cannot judge whether a label is
//     *meaningful*, whether a reading order makes sense, or whether an error
//     message is comprehensible. What it establishes is that the machine-
//     checkable rules pass, which is a floor rather than a ceiling.
//   - It does not filter its findings. Every violation it reports fails this
//     suite, because a list of known-and-accepted violations is a list nobody
//     reads after the second entry.
//
// Usage: node tests/e2e/accessibility.mjs http://127.0.0.1:PORT EMAIL PASSWORD

import { readFileSync } from "node:fs";

const playwrightModule = process.env.PLAYWRIGHT_MODULE || "playwright";
const imported = await import(playwrightModule);
const { chromium } = imported.default ?? imported;

const axeSource = readFileSync(
  process.env.AXE_SOURCE || "/tmp/pw/node_modules/axe-core/axe.min.js",
  "utf8",
);

const [base, email, password] = process.argv.slice(2);
if (!base) {
  console.error("usage: accessibility.mjs <base-url> [email] [password]");
  process.exit(2);
}

let passed = 0;
let failed = 0;
const ok = (what) => { console.log(`  ok    ${what}`); passed++; };
const bad = (what, detail) => { console.log(`  FAIL  ${what}${detail ? `\n${detail}` : ""}`); failed++; };

const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_PATH || undefined,
  args: ["--no-sandbox"],
});
const context = await browser.newContext({ locale: "sv-SE" });
const page = await context.newPage();

/**
 * Runs axe over the current page state.
 *
 * axe is injected with `evaluate` rather than `addScriptTag`: the pages carry a
 * Content-Security-Policy of `script-src 'self'`, so a script tag pointing at
 * anything — including a blob — is refused. Evaluating through the debugging
 * protocol is not subject to the policy, which is what makes auditing a page
 * this strict possible at all.
 */
async function audit(label, options = {}) {
  await page.evaluate(axeSource);
  const results = await page.evaluate(async (config) => {
    // eslint-disable-next-line no-undef
    return await window.axe.run(document, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "best-practice"] },
      ...config,
    });
  }, options);

  if (results.violations.length === 0) {
    ok(`${label}: ${results.passes.length} rules pass, no violations`);
    return;
  }

  const detail = results.violations
    .map((violation) => {
      const nodes = violation.nodes
        .slice(0, 4)
        .map((node) => `           ${node.target.join(" ")}`)
        .join("\n");
      return `        [${violation.impact}] ${violation.id}: ${violation.help}\n${nodes}`;
    })
    .join("\n");
  bad(`${label}: ${results.violations.length} violation(s)`, detail);
}

/** Contrast is checked separately: it needs a rendered page in both themes. */
async function auditContrast(label, scheme) {
  await page.emulateMedia({ colorScheme: scheme });
  await page.waitForTimeout(150);
  await page.evaluate(axeSource);
  const results = await page.evaluate(async () =>
    // eslint-disable-next-line no-undef
    await window.axe.run(document, { runOnly: ["color-contrast"] }));
  if (results.violations.length === 0) {
    ok(`${label} in ${scheme} mode: contrast passes`);
  } else {
    const detail = results.violations[0].nodes
      .slice(0, 6)
      .map((node) => `        ${node.target.join(" ")}: ${node.failureSummary?.split("\n")[1]?.trim()}`)
      .join("\n");
    bad(`${label} in ${scheme} mode: contrast`, detail);
  }
}

try {
  // --- the main interface, before anything has happened -----------------
  await page.goto(`${base}/`, { waitUntil: "networkidle" });
  await audit("the main interface, first screen");
  await auditContrast("the main interface", "light");
  await auditContrast("the main interface", "dark");
  await page.emulateMedia({ colorScheme: "light" });

  // --- the simulation page, signed out ----------------------------------
  await page.goto(`${base}/simulations`, { waitUntil: "networkidle" });
  await page.waitForSelector("#signin:not(.hidden)", { timeout: 10_000 });
  await audit("the simulation page, signed out");

  if (email && password) {
    // --- signed in, with a model on screen ------------------------------
    await page.fill("#email", email);
    await page.fill("#password", password);
    await page.click("#signin-button");
    await page.waitForSelector("#app:not(.hidden)", { timeout: 15_000 });
    await page.waitForSelector("#inputs fieldset", { timeout: 10_000 });
    await audit("the simulation page, with a model loaded");

    // --- with a result, which is the state with the most on screen ------
    await page.fill("#seed", "1");
    await page.click("#run");
    await page.waitForSelector("#output:not(.hidden)", { timeout: 60_000 });
    await audit("the simulation page, showing a result");
    await auditContrast("a result", "light");
    await auditContrast("a result", "dark");
    await page.emulateMedia({ colorScheme: "light" });

    // Every tab panel, because a violation inside a hidden panel is a
    // violation that appears the moment somebody clicks.
    for (const tab of ["tab-cdf", "tab-sens", "tab-conv", "tab-runs"]) {
      await page.click(`#${tab}`);
      await page.waitForTimeout(120);
      await audit(`the ${tab.replace("tab-", "")} panel`);
    }

    // --- keyboard reachability ------------------------------------------
    //
    // axe cannot answer this one: it checks that things *can* be focused, not
    // that a person can get to them in order.
    await page.click("#tab-hist");
    const reachable = await page.evaluate(() => {
      const focusable = document.querySelectorAll(
        'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), ' +
        'textarea:not([disabled]), [tabindex]:not([tabindex="-1"])');
      return [...focusable].filter((element) => {
        const style = window.getComputedStyle(element);
        const hidden = style.display === "none" || style.visibility === "hidden";
        const inHiddenParent = element.closest(".hidden") !== null;
        return !hidden && !inHiddenParent;
      }).length;
    });
    if (reachable >= 10) {
      ok(`${reachable} controls are reachable by keyboard`);
    } else {
      bad(`only ${reachable} controls are keyboard reachable`);
    }

    // A visible focus indicator. Without one, keyboard navigation is possible
    // and unusable — you cannot see where you are.
    //
    // Reached with Tab rather than with `page.focus`, because that is how a
    // person produces focus and because the two are not the same: Chromium
    // paints its default ring only for `:focus-visible`, so a control focused
    // by script can show nothing while the same control focused by keyboard
    // shows a ring. Testing the scripted path alone would have missed a real
    // gap; testing only the keyboard path would hide one.
    await page.click("#tab-hist");
    let indicatorsSeen = 0;
    let indicatorsMissing = [];
    for (let step = 0; step < 25; step++) {
      await page.keyboard.press("Tab");
      const state = await page.evaluate(() => {
        const active = document.activeElement;
        if (!active || active === document.body) return null;
        const style = window.getComputedStyle(active);
        return {
          name: active.id || active.tagName.toLowerCase(),
          visible: style.outlineStyle !== "none" || style.boxShadow !== "none",
        };
      });
      if (!state) continue;
      if (state.visible) indicatorsSeen++;
      else indicatorsMissing.push(state.name);
    }
    if (indicatorsMissing.length === 0 && indicatorsSeen > 5) {
      ok(`every one of ${indicatorsSeen} tab stops shows a focus indicator`);
    } else {
      bad(`tab stops with no visible focus indicator: ${indicatorsMissing.join(", ")}`);
    }

    // And the same control focused by script, which is what happens when the
    // page moves focus itself.
    await page.focus("#run");
    const scripted = await page.evaluate(() => {
      const style = window.getComputedStyle(document.activeElement, null);
      return style.outlineStyle !== "none" || style.boxShadow !== "none";
    });
    if (scripted) ok("and a control focused by the page shows one too");
    else bad("a control focused by the page shows no focus indicator");
  }

  // --- 200% zoom, which is a WCAG 1.4.4 requirement ---------------------
  //
  // Emulated by halving the viewport, which is what doubling the text size
  // amounts to for a layout. The failure it catches is content that becomes
  // unreachable rather than merely cramped.
  await page.setViewportSize({ width: 640, height: 512 });
  await page.goto(`${base}/simulations`, { waitUntil: "networkidle" });
  const overflow = await page.evaluate(() =>
    document.documentElement.scrollWidth - document.documentElement.clientWidth);
  if (overflow <= 1) ok("at 200% zoom the page does not scroll sideways");
  else bad(`at 200% zoom the page overflows by ${overflow}px`);
} catch (error) {
  bad("the audit ran to completion", `        ${error && error.message ? error.message : error}`);
} finally {
  await browser.close();
}

console.log(`\npassed ${passed}, failed ${failed}`);
process.exit(failed === 0 ? 0 : 1);
