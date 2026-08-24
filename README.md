# Skattjakt

**Tax Recovery & Opportunity Engine för svenska aktiebolag.**

Ladda upp ett bokslut; få tillbaka en strukturerad lista på sådant som är värt
att undersöka — skattepositioner, avdrag, periodiseringar, felklassificeringar
och kontrollpunkter — där varje fynd är spårbart till en rad i ett underlag och
en citerad regel.

> Det finns ofta mer att hitta i ett bokslut än man tror.

## Innehåll

| Katalog | Vad det är |
|---|---|
| [`skattjakt/`](skattjakt/) | Motorn. Rust-workspace: regler, pipeline, API, lagring, wasm-bygge för Vercel, infrastruktur och dokumentation. Börja i [`skattjakt/README.md`](skattjakt/README.md). |
| [`kapacitetstest/`](kapacitetstest/) | Scenariokörning mot motorn — 100 syntetiska bokslut, för att mäta täckning och svarstid. |
| [`design-skattjakt/`](design-skattjakt/) | Designunderlag för rapportens presentationslager. |

## Komma igång

```sh
cd skattjakt
cp .env.example .env     # fyll i ANTHROPIC_API_KEY med mera
cargo test --workspace
```

CI kör fmt, clippy, hela testsviten, golden-prov, integrationsprov och
leveranskedjekontroller — se [`.github/workflows/ci.yml`](.github/workflows/ci.yml).
