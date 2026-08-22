# -*- coding: utf-8 -*-
"""Genererar artboards ur samma tokenuppsättning som apps/api/ui/app.css.

Tokens skrivs som literala värden inline i märkningen, inte som CSS-variabler:
det är inline-style-attributen Claude Designs egenskapspanel redigerar, och en
variabel hade gjort varje färg oredigerbar i editorn. Att de ändå inte kan
glida isär beror på att de står en gång här.
"""
import json, pathlib

T = dict(
    bg="#fbfaf8", surface="#ffffff", border="#e3ded6", ink="#1c1a17",
    muted="#6b655c", accent="#1f5d4c", accent_soft="#e8f1ee",
    warn="#8a5a1a", warn_soft="#fbf1e2", radius="10px",
    font='16px/1.55 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif',
)

HELMET = """  <style>
    body {{ margin: 0; background: {bg}; color: {ink};
           font: {font}; }}
    * {{ box-sizing: border-box; }}
    a, a:visited {{ color: {accent}; }}
    a:hover {{ color: #17453a; }}
    h1, h2, h3 {{ margin: 0; }}
    p {{ margin: 0; }}
    ul {{ margin: 0; }}
  </style>""".format(**T)

def head(title):
    return ('<!doctype html>\n<html>\n<head>\n  <meta charset="utf-8">\n'
            '  <script src="./support.js"></script>\n</head>\n<body>\n<x-dc>\n'
            '<helmet>\n' + HELMET + '\n</helmet>\n')

TAIL = "</x-dc>\n</body>\n</html>\n"

def tag(text, kind="accent"):
    """Statusetikett. Samma mått som .tag i app.css."""
    fg, bg = (T["warn"], T["warn_soft"]) if kind == "warn" else (T["accent"], T["accent_soft"])
    return (f'<span style="display: inline-block; font-size: 0.72rem; padding: 0.12rem 0.5rem; '
            f'border-radius: 999px; background: {bg}; color: {fg}; '
            f'border: 1px solid transparent; white-space: nowrap;">{text}</span>')

def icon(kind):
    """Streckade ikoner på 20-rutnät. Inga emoji — de skalar inte och går inte att färga om."""
    paths = {
        "check": '<path d="M4 10.5l4 4 8-9"/>',
        "alert": '<path d="M10 3.5l7 13H3l7-13z"/><path d="M10 8v3.5"/><path d="M10 14h.01"/>',
        "clock": '<circle cx="10" cy="10" r="7"/><path d="M10 6v4.2l2.6 1.6"/>',
        "doc":   '<path d="M5 2.5h6.5L15 6v11.5H5z"/><path d="M11.5 2.5V6H15"/><path d="M7.5 10h5"/><path d="M7.5 13h3.5"/>',
    }
    return (f'<svg width="20" height="20" viewBox="0 0 20 20" fill="none" '
            f'stroke="currentColor" stroke-width="1.5" stroke-linecap="round" '
            f'stroke-linejoin="round" aria-hidden="true" style="flex: none;">{paths[kind]}</svg>')

def card(f, expanded=False):
    """Ett fyndkort. Bevisraden är det som skiljer produkten från en gissning,
    så den ligger öppen på de fynd som faktiskt bär ett belopp.

    Barnen sitter i en flex-kolumn med `gap`, inte på egna marginaler. Det är
    inte kosmetik: i en editor där någon drar om, dubblerar eller raderar
    element överlever gap-avstånd, medan en marginal per barn lämnar hål efter
    det som togs bort.
    """
    deferral = f.get("effect") == "deferral"
    amount_colour = T["warn"] if deferral else T["ink"]
    has_amount = f["impact_display"] != "Ingen beräknad ekonomisk effekt"
    status_kind = "warn" if f["status_label"] in ("Varning", "Undersök") else "accent"

    amount = (f'<span style="font-variant-numeric: tabular-nums; font-weight: 600; '
              f'white-space: nowrap; color: {amount_colour}; font-size: 0.95rem;">'
              f'{f["impact_display"]}</span>') if has_amount else (
              f'<span style="color: {T["muted"]}; font-size: 0.85rem; white-space: nowrap;">'
              f'Inget belopp beräknat</span>')

    evidence = ""
    if expanded and f.get("supporting_values"):
        rows = "".join(
            f'<li style="display: flex; gap: 0.6rem; align-items: baseline;">'
            f'<code style="font-size: 0.78rem; color: {T["muted"]}; flex: none;">{v["kind"]}</code>'
            f'<span style="font-variant-numeric: tabular-nums; font-weight: 600; font-size: 0.82rem;">{v["amount"]}</span>'
            f'<span style="color: {T["muted"]}; font-size: 0.78rem;">sida {v["page"]} — ”{v["excerpt"]}”</span>'
            f'</li>' for v in f["supporting_values"])
        rules = "".join(
            f'<li style="color: {T["muted"]}; font-size: 0.8rem;">{r["source"]} '
            f'<span style="color: {T["warn"]};">(källa ej kontrollerad)</span></li>'
            for r in f.get("rules", []))
        evidence = (
            f'<div style="padding-top: 0.75rem; border-top: 1px solid {T["border"]}; '
            f'display: flex; flex-direction: column; gap: 0.5rem;">'
            f'<div style="display: flex; gap: 0.4rem; align-items: center; color: {T["accent"]}; '
            f'font-size: 0.8rem; font-weight: 550;">{icon("doc")}Underlaget bakom siffran</div>'
            f'<ul style="list-style: none; padding: 0; display: flex; flex-direction: column; gap: 0.3rem;">{rows}</ul>'
            f'<ul style="list-style: circle; padding-left: 1.1rem; '
            f'display: flex; flex-direction: column; gap: 0.2rem;">{rules}</ul>'
            f'</div>')

    missing = ""
    if f.get("missing_information"):
        # Hela listan, inte de tre första. Att kapa den gjorde att avsnittet
        # "Detta skulle göra analysen bättre" kunde peka på ett underlag som
        # kortet självt inte bad om.
        items = "".join(f'<li>{m}</li>' for m in f["missing_information"])
        missing = (
            f'<div style="display: flex; flex-direction: column; gap: 0.25rem;">'
            f'<div style="color: {T["muted"]}; font-size: 0.78rem;">Skulle stärka fyndet</div>'
            f'<ul style="list-style: circle; padding-left: 1.1rem; color: {T["muted"]}; '
            f'font-size: 0.82rem; display: flex; flex-direction: column; gap: 0.2rem;">{items}</ul>'
            f'</div>')

    return (
        f'<article style="background: {T["surface"]}; border: 1px solid {T["border"]}; '
        f'border-radius: {T["radius"]}; padding: 1rem 1.1rem; '
        f'display: flex; flex-direction: column; gap: 0.6rem;">'
        f'<header style="display: flex; justify-content: space-between; gap: 1rem; align-items: baseline;">'
        f'<h3 style="font-size: 1rem; letter-spacing: -0.01em;">{f["title"]}</h3>{amount}</header>'
        f'<div style="display: flex; gap: 0.4rem; align-items: center; flex-wrap: wrap;">'
        f'{tag(f["status_label"], status_kind)}{tag(f["category"])}'
        f'<span style="color: {T["muted"]}; font-size: 0.78rem;">Tillförlitlighet {f["confidence"]} %</span>'
        + (f'{tag("Uppskjuten skatt", "warn")}' if deferral else '') +
        f'</div>'
        f'<p style="color: {T["ink"]}; font-size: 0.9rem;">{f["rationale"]}</p>'
        f'<p style="color: {T["muted"]}; font-size: 0.85rem;">'
        f'<strong style="color: {T["ink"]}; font-weight: 550;">Nästa steg.</strong> {f["recommended_action"]}</p>'
        + missing + evidence + '</article>')

def potential(ep):
    """Sänkt skatt och uppskjuten skatt, åtskilda. Det är hela poängen med
    avsnittet: en läsare som ser ett tal läser det som pengar att hämta."""
    deferred = ""
    if ep["deferred"]["high"] > 0:
        deferred = (
            f'<div style="border-left: 3px solid {T["warn"]}; background: {T["warn_soft"]}; '
            f'padding: 0.8rem 1rem; border-radius: 0 8px 8px 0;">'
            f'<div style="display: flex; justify-content: space-between; gap: 1rem; align-items: baseline;">'
            f'<span style="font-size: 0.85rem; color: {T["warn"]}; font-weight: 550;">Uppskjuten skatt</span>'
            f'<span style="font-variant-numeric: tabular-nums; font-weight: 600; font-size: 1.1rem; '
            f'color: {T["warn"]}; white-space: nowrap;">{ep["deferred_display"]}</span></div>'
            f'<p style="color: {T["muted"]}; font-size: 0.8rem; text-wrap: pretty;">'
            f'{ep["deferred_note"]}</p></div>')
    return (
        f'<div style="display: flex; flex-direction: column; gap: 0.7rem;">'
        f'<div style="background: {T["accent_soft"]}; border-radius: 8px; padding: 0.9rem 1rem;">'
        f'<div style="display: flex; justify-content: space-between; gap: 1rem; align-items: baseline;">'
        f'<span style="font-size: 0.85rem; color: {T["accent"]}; font-weight: 550;">Lägre skatt</span>'
        f'<span style="font-variant-numeric: tabular-nums; font-weight: 600; font-size: 1.35rem; '
        f'color: {T["accent"]}; white-space: nowrap;">{ep["display"]}</span></div>'
        f'<p style="color: {T["muted"]}; font-size: 0.8rem; text-wrap: pretty;">{ep["note"]}</p>'
        f'</div>{deferred}</div>')

def section(n, title, body, lede=None):
    l = (f'<p style="color: {T["muted"]}; font-size: 0.88rem; text-wrap: pretty;">{lede}</p>') if lede else ""
    return (f'<section style="display: flex; flex-direction: column; gap: 0.75rem;">'
            f'<h2 style="font-size: 1.15rem; letter-spacing: -0.01em;">'
            f'<span style="color: {T["muted"]}; font-weight: 400;">{n}.</span> {title}</h2>'
            f'{l}{body}</section>')

def stat(label, value, colour=None):
    return (f'<div style="display: flex; flex-direction: column; gap: 0.1rem;">'
            f'<span style="font-variant-numeric: tabular-nums; font-weight: 600; font-size: 1.5rem; '
            f'color: {colour or T["ink"]};">{value}</span>'
            f'<span style="color: {T["muted"]}; font-size: 0.78rem;">{label}</span></div>')

def page(company, year, ruleset, audience_label, sections, disclaimer):
    return (
        f'<main style="max-width: 760px; margin: 0 auto; padding: 2.5rem 1.25rem 3.5rem; '
        f'display: flex; flex-direction: column; gap: 2rem;">'
        f'<header style="display: flex; flex-direction: column; gap: 0.4rem;">'
        f'<div style="display: flex; gap: 0.5rem; align-items: center;">{tag(audience_label)}</div>'
        f'<h1 style="font-size: 1.9rem; letter-spacing: -0.02em;">Din Skattjakt</h1>'
        f'<p style="color: {T["muted"]}; font-size: 0.9rem;">'
        f'<strong style="color: {T["ink"]}; font-weight: 550;">{company}</strong> · räkenskapsår {year} '
        f'· regelverk {ruleset}</p></header>'
        + "".join(sections) +
        f'<footer style="border-top: 1px solid {T["border"]}; padding-top: 1.2rem; '
        f'font-size: 0.78rem; color: {T["muted"]}; text-wrap: pretty;">{disclaimer}</footer>'
        f'</main>')

# Riktig motorutdata, inte påhittade siffror. Genererad med:
#
#   skattjakt-analyze --format json --audience <lager> \
#       --profile examples/exempel-profil.json examples/exempel-bokslut.txt
#
# Sparad i repot så designen går att återskapa utan att först bygga motorn.
HERE = pathlib.Path(__file__).parent
R = {a: json.loads((HERE / "data" / f"rapport-{a}.json").read_text(encoding="utf-8"))
     for a in ("company", "private", "accountant")}

def report_sections(aud, findings, expand_titles):
    s = R[aud]["sections"]
    su = s["summary"]
    stats = (
        f'<div style="display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 1rem; '
        f'background: {T["surface"]}; border: 1px solid {T["border"]}; border-radius: {T["radius"]}; '
        f'padding: 1.1rem;">'
        + stat("hög prioritet", su["high_priority_count"], T["ink"])
        + stat("bör undersökas", su["should_investigate_count"])
        + stat("kräver mer underlag", su["needs_more_evidence_count"])
        + stat("varningar om underlaget", su["warnings_count"]) + '</div>')
    summary = section(1, "Sammanfattning",
        f'<div style="display: flex; flex-direction: column; gap: 0.9rem;">'
        f'<p style="font-size: 1.05rem; color: {T["ink"]}; text-wrap: pretty;">{su["headline"]}</p>'
        f'{stats}</div>')

    first = s["start_here"][0]
    start = section(2, "Börja här",
        f'<div style="border: 1px solid {T["accent"]}; background: {T["accent_soft"]}; '
        f'border-radius: {T["radius"]}; padding: 1rem 1.1rem; display: flex; gap: 0.75rem;">'
        f'<span style="color: {T["accent"]};">{icon("alert")}</span>'
        f'<div style="display: flex; flex-direction: column; gap: 0.35rem;">'
        f'<h3 style="font-size: 1rem;">{first["title"]}</h3>'
        f'<p style="font-size: 0.88rem; color: {T["ink"]}; text-wrap: pretty;">{first["recommended_action"]}</p>'
        f'</div></div>',
        "Det som inte går att rätta i efterhand kommer först.")

    cards = "".join(card(f, expanded=f["title"] in expand_titles) for f in findings)
    opps = section(3, "Potentiella möjligheter",
        f'<div style="display: flex; flex-direction: column; gap: 0.85rem;">{cards}</div>')

    mi = s["missing_information"][:6]
    rows = "".join(
        f'<li style="display: flex; gap: 0.75rem; align-items: baseline; padding: 0.55rem 0; '
        f'border-bottom: 1px solid {T["border"]};">'
        f'<span style="flex: 1; font-size: 0.9rem;">{m["description"]}</span>'
        f'<span style="color: {T["muted"]}; font-size: 0.78rem; text-align: right; flex: none; '
        f'max-width: 46%;">{m["unlocks"]}</span></li>' for m in mi)
    missing = section(5, "Detta skulle göra analysen bättre",
        f'<div style="display: flex; flex-direction: column; gap: 0.75rem;">'
        f'<ul style="list-style: none; padding: 0; margin: 0;">{rows}</ul>'
        f'<p style="color: {T["muted"]}; font-size: 0.8rem;">'
        f'Ytterligare {len(s["missing_information"]) - len(mi)} poster i den fullständiga rapporten.</p></div>',
        "Sorterat efter hur många fynd varje underlag stärker.")

    w = s["warnings"]
    if w:
        wrows = "".join(
            f'<li style="border-left: 3px solid {T["warn"]}; background: {T["warn_soft"]}; '
            f'padding: 0.7rem 1rem; border-radius: 0 8px 8px 0; font-size: 0.88rem;">'
            f'{x["message"]}</li>' for x in w)
        wbody = (f'<ul style="list-style: none; padding: 0; display: flex; '
                 f'flex-direction: column; gap: 0.5rem;">{wrows}</ul>')
    else:
        wbody = (f'<p style="color: {T["muted"]}; font-size: 0.88rem;">'
                 f'Inget i underlaget motsäger sig självt. Varningar här gäller '
                 f'dokumenten — två olika värden för samma post, en balansräkning '
                 f'som inte går ihop — och är något annat än ett fynd med statusen '
                 f'”Varning”.</p>')
    warnings = section(4, "Att kontrollera i underlaget", wbody)

    econ = section(6, "Ekonomisk potential", potential(s["economic_potential"]))
    return summary, start, opps, warnings, missing, econ, s

EXPAND = {"Skattemässigt avskrivningsutrymme på inventarier", "Periodiseringsfond"}

def write(name, title, inner):
    pathlib.Path(name).write_text(head(title) + inner + "\n" + TAIL, encoding="utf-8")
    print(f"  {name}")

# ---- Bolagsanalys, huvudartboarden -----------------------------------------
c = R["company"]["sections"]
summary, start, opps, warnings, missing, econ, _ = report_sections("company", c["opportunities"], EXPAND)
write("Main.dc.html", "Bolagsanalys",
      page("Exempelbolaget AB", "2025", "se-2025.1", "Bolagsanalys · 69 kr",
           [summary, start, opps, warnings, missing, econ],
           "Skattjakt är ett analys- och upptäcktsverktyg. Resultaten är preliminära och ska inte "
           "betraktas som juridisk rådgivning, revisionsuttalande, skattebesked eller garanti om "
           "skatteåterbäring eller besparing. Identifierade möjligheter bör verifieras mot aktuella "
           "regler och företagets fullständiga underlag innan någon åtgärd vidtas."))

# ---- Privatanalys -----------------------------------------------------------
p = R["private"]["sections"]
psummary, pstart, popps, pwarnings, pmissing, pecon, _ = report_sections(
    "private", p["opportunities"], EXPAND)
write("Privatanalys.dc.html", "Privatanalys",
      page("Exempelbolaget AB", "2025", "se-2025.1", "Privatanalys · 29 kr",
           [psummary, pstart, popps, pwarnings, pmissing, pecon],
           "Skattjakt är ett analys- och upptäcktsverktyg. Resultaten är preliminära och ska inte "
           "betraktas som juridisk rådgivning eller skattebesked. Kontrollera varje möjlighet mot "
           "ditt fullständiga underlag innan du agerar."))

# ---- Skattjakt Kontroll, redovisningsbyråns lager ---------------------------
#
# Det enda som faktiskt skiljer lagren i motorn idag är den här genomgången och
# friskrivningen. Designen påstår inte mer än så — se anteckningen på canvasen.
a = R["accountant"]["sections"]
cr = a["control_review"]

def band(title, items, colour, bg, note, right):
    """Ett band i kontrollgenomgången.

    De fyra banden bär olika uppgifter — ett fynd har tillförlitlighet, en
    prövad regel har ett skäl, en samtalspunkt har ett belopp — så högerkolumnen
    kommer in som en funktion i stället för att tvingas in i samma form.
    """
    rows = "".join(
        f'<li style="display: flex; justify-content: space-between; gap: 1rem; align-items: baseline; '
        f'padding: 0.5rem 0; border-bottom: 1px solid {T["border"]};">'
        f'<span style="font-size: 0.9rem;">{i.get("title") or i["summary"]}</span>'
        f'<span style="color: {T["muted"]}; font-size: 0.78rem; text-align: right; flex: none; '
        f'max-width: 52%;">{right(i)}</span></li>' for i in items)
    empty = (f'<p style="color: {T["muted"]}; font-size: 0.82rem;">'
             f'Inget i den här kategorin.</p>') if not items else ""
    return (f'<div style="border: 1px solid {T["border"]}; border-left: 3px solid {colour}; '
            f'border-radius: 0 {T["radius"]} {T["radius"]} 0; background: {bg}; padding: 0.9rem 1.1rem; '
            f'display: flex; flex-direction: column; gap: 0.5rem;">'
            f'<div style="display: flex; flex-direction: column; gap: 0.2rem;">'
            f'<div style="display: flex; justify-content: space-between; gap: 1rem; align-items: baseline;">'
            f'<h3 style="font-size: 0.95rem; color: {colour};">{title}</h3>'
            f'<span style="font-variant-numeric: tabular-nums; font-weight: 600; color: {colour};">'
            f'{len(items)}</span></div>'
            f'<p style="color: {T["muted"]}; font-size: 0.78rem;">{note}</p></div>'
            + (f'<ul style="list-style: none; padding: 0;">{rows}</ul>' if items else empty)
            + '</div>')

review = section(3, "Kontrollgenomgång",
    '<div style="display: flex; flex-direction: column; gap: 0.85rem;">'
    + band("Måste kontrolleras", cr["must_check"], T["warn"], T["warn_soft"],
           "Fynd som måste stämmas av innan bokslutet lämnas in.",
           lambda i: f'{i["category"]} · {i["confidence"]} %')
    + band("Möjlig förbättring", cr["possible_improvement"], T["accent"], T["accent_soft"],
           "Prövat och bedömt som en möjlighet värd att ta upp.",
           lambda i: i["category"])
    + band("Prövat, ser korrekt ut", cr["looks_correct"], T["muted"], T["surface"],
           "Regler som prövats mot underlaget utan att slå till. Namnet är regelns "
           "larm — att det inte utlöstes är beskedet.",
           lambda i: "slog inte till")
    + band("Värt att ta upp med kunden", cr["worth_raising"], T["muted"], T["surface"],
           "Inget att rätta, men något klienten bör känna till.",
           lambda i: f'{i["area"]} · {i["impact_display"]}')
    + '</div>',
    "Samma analys som bolagsrapporten, ordnad efter vad en granskare gör med den.")

ksummary, kstart, kopps, kwarnings, kmissing, kecon, _ = report_sections(
    "accountant", a["opportunities"], EXPAND)
write("Kontroll.dc.html", "Skattjakt Kontroll",
      page("Exempelbolaget AB", "2025", "se-2025.1", "Skattjakt Kontroll · 69 kr",
           [ksummary, kstart, review, kwarnings, kmissing, kecon],
           "Skattjakt är ett analys- och upptäcktsverktyg och ersätter inte byråns egen granskning. "
           "Resultaten är preliminära och utgör varken revisionsuttalande eller skattebesked. Varje "
           "fynd ska verifieras mot fullständigt underlag innan det förs vidare till klienten."))

# ---- Komponentark ----------------------------------------------------------
#
# Fyndkortet i sina tillstånd, plus byggstenarna var för sig. Här ändras
# etikettens form eller bevisradens täthet en gång i stället för nio.
def spec(label, note, body):
    return (f'<div style="display: flex; flex-direction: column; gap: 0.5rem;">'
            f'<div style="display: flex; gap: 0.6rem; align-items: baseline;">'
            f'<h2 style="font-size: 0.85rem; letter-spacing: 0.04em; text-transform: uppercase; '
            f'color: {T["muted"]};">{label}</h2>'
            f'<span style="color: {T["muted"]}; font-size: 0.8rem;">{note}</span></div>'
            f'{body}</div>')

by_title = {f["title"]: f for f in c["opportunities"]}
sheet = (
    f'<main style="max-width: 900px; margin: 0 auto; padding: 2.5rem 1.25rem 3.5rem; '
    f'display: flex; flex-direction: column; gap: 2rem;">'
    f'<header style="display: flex; flex-direction: column; gap: 0.4rem;">'
    f'<h1 style="font-size: 1.9rem; letter-spacing: -0.02em;">Byggstenar</h1>'
    f'<p style="color: {T["muted"]}; font-size: 0.9rem; text-wrap: pretty;">'
    f'Fyndkortet bär hela produktens trovärdighet: utan bevisraden är det en gissning med '
    f'typografi. Tillstånden nedan är de fyra som ett fynd faktiskt kan ha.</p></header>'
    + spec("Fynd med belopp och underlag", "det enda tillstånd som får bära en siffra",
           card(by_title["Skattemässigt avskrivningsutrymme på inventarier"], expanded=True))
    + spec("Uppskov", "beloppet finns, men räknas inte in i potentialen",
           card(by_title["Periodiseringsfond"], expanded=True))
    + spec("Fynd utan belopp", "regeln slog till, men ingen siffra går att härleda",
           card(by_title["Avdrag för pensionskostnader"]))
    + spec("Kräver mer underlag", "för svagt för att presenteras som något att agera på",
           card(by_title["Moms på leasing av personbil"]))
    + spec("Etiketter", "två stilar, tre betydelser — status delar utseende med de andra",
           f'<div style="display: flex; flex-direction: column; gap: 0.6rem;">'
           + "".join(
               f'<div style="display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap;">'
               f'<span style="color: {T["muted"]}; font-size: 0.78rem; width: 7.5rem; flex: none;">{lbl}</span>'
               f'{pills}</div>'
               for lbl, pills in [
                   ("Status", f'{tag("Möjlighet")}{tag("Verifiera")}{tag("Undersök", "warn")}{tag("Varning", "warn")}'),
                   ("Kategori", f'{tag("Skatt")}{tag("Investeringar")}{tag("Personal")}{tag("Moms")}'),
                   ("Effekt", f'{tag("Uppskjuten skatt", "warn")}'),
               ])
           + f'<p style="color: {T["muted"]}; font-size: 0.8rem; text-wrap: pretty;">'
             f'”Undersök” och ”Varning” är idag omöjliga att skilja åt, och en '
             f'kategori ser ut som en status. Om du vill att de ska gå isär är '
             f'det här stället att göra det på.</p></div>')
    + spec("Ekonomisk potential", "sänkt och uppskjuten skatt, aldrig i samma tal",
           potential(c["economic_potential"]))
    + '</main>')
write("Byggstenar.dc.html", "Byggstenar", sheet)
