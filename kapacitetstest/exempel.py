# -*- coding: utf-8 -*-
"""Tio av de hundra, valda för att visa spännvidden — inte för att imponera."""
import json, pathlib
from scenarios import SCENARIOS

VAL = [
    ("001", "Vardagsfallet: en konsultbyrå med normalt år"),
    ("009", "Fastighetsbolaget — det som förut fick 630 000 kr ur luften"),
    ("027", "Verkstadsbolag, rekordår efter tidigare underskott"),
    ("068", "Samma bokslut, belopp i tusental kronor"),
    ("035", "Alla profilfrågor obesvarade"),
    ("056", "Alla profilfrågor besvarade — samma bokslut"),
    ("061", "Avskrivningsraden saknas i underlaget"),
    ("089", "Promptinjektion mitt i årsredovisningen"),
    ("086", "Balansräkningen går inte ihop"),
    ("096", "Räkenskapsår 2026 — utanför regelverket"),
]
res = {x["id"]: x for x in json.load(open("resultat.json"))}
scen = {s["id"]: s for s in SCENARIOS}

def kr(ore): return f"{ore/100:,.0f}".replace(",", " ") + " kr"

out = []
for i, (sid, rubrik) in enumerate(VAL, 1):
    s, r = scen[sid], res[sid]
    out.append(f"\n{'='*78}\nEXEMPEL {i} av 10 — scenario {sid} ({r['group']})\n{rubrik}\n{'='*78}")
    p = s["profile"]
    prof = ", ".join(f"{k}={v}" for k, v in p.items()
                     if k not in ("name", "org_number", "fiscal_year_start", "fiscal_year_end"))
    out.append(f"Bolag   : {p['name']}, räkenskapsår {p['fiscal_year_start'][:4]}")
    out.append(f"Profil  : {prof or '— inga frågor besvarade'}")
    doclines = [l for l in s["doc"].split("\n") if l.strip()]
    out.append(f"Underlag: {len(doclines)} rader")
    for l in doclines[:6]:
        out.append(f"          {l[:70]}")
    if len(doclines) > 6:
        out.append(f"          … {len(doclines)-6} rader till")
    out.append(f"Väntat  : {s['expect']}")
    out.append("")
    if not r.get("ok"):
        out.append(f"UTFALL  : AVVISAD ({r['ms']} ms)")
        out.append(f"          {r.get('reason','')}")
        out.append("")
        continue
    rep = json.loads(pathlib.Path(f"resultat/{sid}.json").read_text())["sections"]
    out.append(f"UTFALL  : {r['ms']} ms")
    out.append(f"          {rep['summary']['headline']}")
    ep = rep["economic_potential"]
    out.append(f"          Lägre skatt      {ep['display']}")
    if ep["deferred"]["high"]:
        out.append(f"          Uppskjuten skatt {ep['deferred_display']}")
    if rep["warnings"]:
        for w in rep["warnings"]:
            out.append(f"          VARNING [{w['code']}] {w['message'][:62]}")
    out.append("")
    out.append("          Fynd:")
    for o in rep["opportunities"]:
        belopp = o["impact_display"] if o["impact_display"] != "Ingen beräknad ekonomisk effekt" else "—"
        out.append(f"            {o['status_label']:<10} {o['title'][:44]:<46} {belopp}")
    ev = [(v["kind"], v["amount"], v["excerpt"][:40])
          for o in rep["opportunities"] for v in o["supporting_values"]]
    if ev:
        out.append("")
        out.append("          Underlag som citerades:")
        seen = set()
        for k, a, x in ev:
            if k in seen: continue
            seen.add(k)
            out.append(f"            {k:<20} {a:<20} ← ”{x}”")
    out.append("")
    out.append(f"          Frågar efter {len(rep['missing_information'])} saker till, t.ex.:")
    for m in rep["missing_information"][:3]:
        out.append(f"            {m['description'][:52]:<54} {m['unlocks'][:34]}")
    out.append("")

pathlib.Path("EXEMPEL.txt").write_text("\n".join(out), encoding="utf-8")
print("\n".join(out))
