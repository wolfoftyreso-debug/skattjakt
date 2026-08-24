# -*- coding: utf-8 -*-
"""Kör alla hundra och samlar utfallet. Inget här dömer — det gör analysen."""
import json, pathlib, subprocess, sys, time
from scenarios import SCENARIOS

# Whichever build is newer, never whichever profile is preferred — the same
# rule the shell suites use, and for the same reason: a stale binary produced
# four false diagnoses in this repository before anybody wrote the rule down.
def binary():
    root = pathlib.Path(__file__).resolve().parent.parent / "skattjakt"
    found = [p for p in (root / "target" / profile / "skattjakt-analyze"
                         for profile in ("release", "debug")) if p.exists()]
    if not found:
        raise SystemExit(
            "build the analysis command first:\n"
            "  cd skattjakt && cargo build --release --bin skattjakt-analyze")
    return str(max(found, key=lambda p: p.stat().st_mtime))

BIN = binary()
WORK = pathlib.Path("kor"); WORK.mkdir(exist_ok=True)
OUT = pathlib.Path("resultat"); OUT.mkdir(exist_ok=True)

results = []
t0 = time.time()
for s in SCENARIOS:
    doc = WORK / f"{s['id']}.txt"; doc.write_text(s["doc"], encoding="utf-8")
    prof = WORK / f"{s['id']}.json"; prof.write_text(json.dumps(s["profile"], ensure_ascii=False))
    started = time.time()
    p = subprocess.run([BIN, "--format", "json", "--profile", str(prof), *s["args"], str(doc)],
                       capture_output=True, text=True, timeout=120)
    ms = int((time.time() - started) * 1000)
    rec = {"id": s["id"], "group": s["group"], "name": s["name"], "expect": s["expect"],
           "exit": p.returncode, "ms": ms, "stderr": p.stderr.strip()[:400]}
    if p.returncode == 0 and p.stdout.strip():
        try:
            rep = json.loads(p.stdout)
            (OUT / f"{s['id']}.json").write_text(p.stdout, encoding="utf-8")
            sec = rep["sections"]
            opps = sec["opportunities"]
            rec.update({
                "ok": True,
                "findings": len(opps),
                "rules": sorted({r["rule_id"] for o in opps for r in o["rules"]}),
                "titles": [o["title"] for o in opps],
                "statuses": sorted({o["status_label"] for o in opps}),
                "lower_low": sec["economic_potential"]["total"]["low"],
                "lower_high": sec["economic_potential"]["total"]["high"],
                "deferred_low": sec["economic_potential"]["deferred"]["low"],
                "deferred_high": sec["economic_potential"]["deferred"]["high"],
                "warnings": [w["code"] for w in sec["warnings"]],
                "missing": len(sec["missing_information"]),
                "facts": sorted({v["kind"] for o in opps for v in o["supporting_values"]}),
                "limitations": len(sec["limitations"]),
                "raw_len": len(p.stdout),
            })
        except json.JSONDecodeError as e:
            rec.update({"ok": False, "parse_error": str(e)})
    else:
        rec.update({"ok": False})
    results.append(rec)
    print(f"{s['id']} {s['group']} exit={p.returncode:<3} {ms:>5}ms  {s['name'][:58]}", flush=True)

pathlib.Path("resultat.json").write_text(json.dumps(results, ensure_ascii=False, indent=1))
print(f"\n{len(results)} scenarier på {time.time()-t0:.1f}s")
