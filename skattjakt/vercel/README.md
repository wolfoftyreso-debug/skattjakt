# Skattjakt på Vercel

Analysmotorn kompilerad till WebAssembly, serverad av Vercel-funktioner. Ingen
långlivad process, ingen jobbkö, ingenting att polla.

```
POST /api/analyse             en analys, inline
POST /api/payments/callback   Swish säger att något hänt (oautentiserad, med flit)
GET  /api/cron/reconcile      avstämningssvepet, var femte minut
GET  /api/health              vilket regelverk, och vad som är konfigurerat
```

## Varför det här fungerar

Den ursprungliga tjänsten var en axum-server plus två workers som leasade jobb
ur en Postgres-kö. Inget av det har någonstans att köra på Vercel.

Att produkten ändå passar är en **mätning, inte ett argument**: hundra
genererade scenarier kördes på median 3 ms nativt och **1,66 ms genom
WASM-modulen** (p95 2,71 ms, kallstart 38,8 ms). Kön fanns för en pipeline vars
långsamma steg var ett modellanrop. Utan ett — och en regelbaserad analys är
hela produkten tills någon kvalificerad granskat regelverket — finns det
ingenting att köa. Analysen körs inne i requesten och blir klar innan en
pollning hunnit sättas upp.

Motorn korsar till `wasm32-unknown-unknown` orörd. Det som **inte** korsar är
allt som öppnar en socket: Anthropic-klienten och OTLP-exporten ligger bakom
featuren `native` och finns helt enkelt inte här. Reglerna, extraktionen,
konfidensmodellen och rapporten är samma kod som den native binären kör, och
samma prov täcker båda.

## Komma igång

```bash
npm install
npm run build          # kompilerar motorn till WASM och skriver JS-bindningen
npm run check          # tio prov genom bryggan
vercel dev
```

`scripts/build-wasm.sh` installerar Rust och `wasm-bindgen-cli` om de saknas,
båda pinnade. En verktygskedja som glider är ett regelverk som räknar olika
mellan två deployer av samma commit.

## Miljövariabler

| Variabel | Krävs | Vad den styr |
|---|---|---|
| `DATABASE_URL` | ja | Postgres. Neon eller Vercel Postgres. Saknas den är det ett hårt fel, inte ett "kör utan lagring". |
| `SKATTJAKT_PAYMENTS_REQUIRED` | nej | **Defaultar till på när Swish är konfigurerat.** Se nedan. |
| `SWISH_PAYEE_ALIAS` | för betalning | Swish-numret. |
| `SWISH_CERT_PEM` / `SWISH_KEY_PEM` | för betalning | Klientcertifikatet, som PEM i variabeln. En Vercel-funktion har ingen disk värd namnet, och Nodes https-agent tar PEM direkt. |
| `SWISH_CALLBACK_URL` | för betalning | Måste peka på `/api/payments/callback`. |
| `CRON_SECRET` | ja | Vercel signerar cron-anrop med den. Utan kontrollen är rutten ett sätt för vem som helst att få oss att ringa Swish femtio gånger. |
| `ANTHROPIC_API_KEY` | nej | Inte läst av WASM-modulen — se **Vad som inte flyttade**. |

### Betalgrinden defaultar nu åt andra hållet

I den gamla tjänsten defaultade `SKATTJAKT_PAYMENTS_REQUIRED` till **false**, så
en driftsättning som glömde en variabel visade priser och gav bort produkten.
Det var uppmätt, inte misstänkt: `POST /v1/analyses/stored` svarade 202 utan
order mot en butik som visade 69 kr.

Här krävs betalning så fort en leverantör är konfigurerad, och en operatör som
kör internt stänger av det explicit.

## Vad som flyttade, och hur

| Fanns | Nu |
|---|---|
| axum-server | Vercel-funktioner, en per rutt |
| analysis-worker (jobbkö) | inline i requesten — 1,66 ms |
| avstämningssvep i workern | `/api/cron/reconcile`, var femte minut |
| Postgres med FORCE RLS | oförändrad, se nedan |
| mutual TLS mot Swish | Nodes `https.Agent` med PEM ur miljön |
| blob-lagring på filsystem | Vercel Blob |

**Radsäkerheten överlever intakt**, och skälet är värt att säga: tenanten sätts
med `set_config('skattjakt.company_id', $1, true)`, där tredje argumentet gör
den **transaktionsbunden**. En transaktionspoolad anslutning (Neons pooler,
PgBouncer) ger nästa transaktion en annan backend — vilket hade brutit en
sessionsbunden `SET`, och inte bryter den här. Isoleringsmodellen var redan
kompatibel med serverless. Ingen planerade det, men det håller.

## Vad som inte flyttade

Ärligt, för att det är sådant du kommer att märka annars.

- **Språkmodellen.** WASM-modulen har ingen HTTP-klient, så modellens genomgång
  och motsägelsekontroll körs inte. Rapporten **säger det** i avsnitt 9. Vill du
  ha modellen tillbaka måste anropet göras från funktionen i JS och resultatet
  matas in i pipelinen — motorn har redan gränssnittet, men det är inte gjort.
- **OTLP-tracing.** Exporten ligger bakom `native`. Vercels egen loggning finns,
  men den distribuerade tracingen mellan API och worker är borta med workern.
- **De två workarna som processer.** Notifieringsworkern har ingen motsvarighet
  här alls; e-post får skickas från en funktion eller en tjänst.
- **Kubernetes-manifesten och Dockerfilen.** De ligger kvar i repot och
  beskriver en driftsättning som inte är den här. De är inte fel — de är en
  annan väg som fortfarande fungerar.

## Prov

`npm run check` kör tio prov genom bryggan. De duplicerar inte Rust-proven —
de täcker att modulen laddar, att rapporten kommer tillbaka som ett objekt och
inte en `Map`, att en trasig begäran blir ett felvärde och inte en panik över
gränssnittet, och att siffrorna är desamma som den native binären ger.

Det sista provet matar in `IGNORE ALL PREVIOUS INSTRUCTIONS` i årsredovisningen
och kontrollerar att beloppet är oförändrat.
