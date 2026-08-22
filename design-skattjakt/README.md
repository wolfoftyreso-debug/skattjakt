# Skattjakt-rapporten — designunderlag

Artboards för rapportens tre presentationslager, plus ett ark med fyndkortets
tillstånd. Redigeras i Claude Design; källan här är det som seedas in.

## Filerna

| Fil | Vad |
|---|---|
| `build.py` | Genererar artboardsen ur en gemensam tokenuppsättning |
| `Main.dc.html` | Bolagsanalys, den fullständiga rapporten |
| `Privatanalys.dc.html` | Privatlagret |
| `Kontroll.dc.html` | Redovisningsbyråns lager, med kontrollgenomgången |
| `Byggstenar.dc.html` | Fyndkortets fyra tillstånd, etiketter, potentialblocket |
| `canvas.json` | Placering på canvasen, plus anteckningarna |
| `data/rapport-*.json` | Riktig motorutdata som innehållet byggs av |
| `measure.mjs` | Mäter artboardhöjder i Chromium, för `h` i `canvas.json` |

## Varför tokens står literalt

Färgerna och måtten är hämtade ur `skattjakt/apps/api/ui/app.css` och skrivs
literalt i märkningen, inte som CSS-variabler. Det är inline-style-attributen
Claude Designs egenskapspanel redigerar — en variabel hade gjort varje färg
oredigerbar i editorn. Att de ändå inte kan glida isär beror på att de står en
gång, överst i `build.py`.

## Bygga om

```bash
python3 build.py                 # skriver om artboardsen
node measure.mjs                 # höjder, om innehållet växt
```

Sedan seedas canvasen om från artboardsen och publiceras på nytt. Den seedade
HTML-filen är genererad och ligger i `.gitignore`.

## Innehållet är riktigt

`data/rapport-*.json` är utdata från analysmotorn, inte påhittade siffror:

```bash
skattjakt-analyze --format json --audience company \
    --profile examples/exempel-profil.json examples/exempel-bokslut.txt
```

Beloppen i designen — 0–56 650 kr lägre skatt, 0–84 460 kr uppskjuten — och
bevisradernas citat ur bokslutet kommer därifrån.
