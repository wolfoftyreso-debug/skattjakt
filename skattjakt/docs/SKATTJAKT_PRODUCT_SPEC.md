# Skattjakt — Product Specification

## What it is

A company uploads its preliminary or final year-end accounts and gets a
structured list of things worth looking into: potential tax positions,
deductions, accruals, misclassifications, and control points.

The core idea: **there is usually more in a set of accounts than people think,
and nobody currently owns the job of looking.** Bookkeeping closes the year and
files the return. Systematically hunting for what was missed has no owner.
Skattjakt is that owner.

## What it is not

Not a bookkeeping system, a filing system, an audit, an accounting firm, or a
legal adviser. It issues no binding tax opinion and guarantees no saving.

## The governing principle

Skattjakt does not say:

> You are entitled to 186 000 kr.

It says:

> We found a potential tax position that may be worth roughly 120 000–186 000 kr
> to look into.

Every result separates what the system *knows*, what it *observed*, what follows
from a *verifiable rule*, what is a *calculation*, what was *assumed*, what is
*missing*, and what the user should *do next*.

## The flow

**1 — Company.** Name, organisationsnummer, fiscal year, and a small number of
questions about the business. Onboarding asks few things and follows up only
when an answer is actually needed. An unanswered question is *unknown*, never
*no*: a rule that depends on it becomes undecidable and the question appears in
"missing information".

**2 — Documents.** Preliminary or final accounts. PDF today; the pipeline is
typed for CSV, SIE, XLSX, ledgers, tax account statements, asset registers and
payroll summaries.

**3 — Analysis.** Progress is shown as the stages the system actually runs:
reading the accounts, understanding the structure, identifying relevant areas,
searching for opportunities, checking rules, attempting falsification, verifying
calculations, prioritising.

**4 — Result: *Din Skattjakt*.**

```
11 saker att undersöka

3 hög prioritet
5 bör undersökas
3 kräver mer underlag

Potentiell ekonomisk påverkan
125 000–410 000 SEK
```

## Where it looks

Tax, costs, VAT, personnel, investments, R&D — and risk: things that do not save
money but should be checked. A balance sheet that does not balance, untaxed
reserves with no note, an inconsistency between statements.

## Status levels

| Status | Meaning |
|---|---|
| **Identifierad** | Strong evidence and a reviewed rule |
| **Undersök** | A potential opportunity, information missing |
| **Verifiera** | The rule or calculation needs checking |
| **Varning** | Should be reviewed, whether or not it saves money |
| **Avvisad** | Did not survive falsification; retained for audit |

Everything the current rule set produces lands at **Verifiera** or below, because
that rule set has not been reviewed by a qualified adviser. This is enforced in
code, not by convention.

## The evidence card

Every finding shows: the potential economic effect as a range; why it was
flagged; which document values support it and on which page; what is missing;
the recommended next step; confidence; risk; status. Every one of those is
traceable to a document version, a page, a line of text, a rule, and a
calculation.

## Economic effect

Always an interval, never a figure. There is no type in the system capable of
expressing a single-figure estimate.

## Priority

Not money alone. Economic effect, scaled by confidence and relevance, divided by
the effort to investigate, lifted by urgency. A large but unactionable finding
never reaches high priority.

## Finding nothing is a result

When nothing clears the bar, the user does not get an empty page. They get:

> Skattjakten hittade inga tydliga möjligheter på det underlag vi fått.
>
> Det betyder inte att det inte finns möjligheter. Det betyder att vi inte har
> tillräckligt stark evidens för att flagga dem.

...along with which areas were checked, how many rules ran in each, which
documents were analysed, and what further material would help. This builds more
trust than a padded list.

## The real outcome

Not a filing, and not a decision. Three questions to take to your accountant.
That is the strongest thing this product does: it helps a business owner ask
better questions of the person who is actually qualified to answer them.

## Tone

Swedish, concrete, calm, curious, confident, plain. No AI hype, no legalese, no
aggressive tax-optimisation framing, no promises.

Core message: **Det finns ofta mer att hitta i ett bokslut än man tror.**

## Disclaimer

Always available, never in the way:

> Skattjakt är ett analys- och upptäcktsverktyg. Resultaten är preliminära och
> ska inte betraktas som juridisk rådgivning, revisionsuttalande, skattebesked
> eller garanti om skatteåterbäring eller besparing. Identifierade möjligheter
> bör verifieras mot aktuella regler och företagets fullständiga underlag innan
> någon åtgärd vidtas.

Held in one place in code (`skattjakt_core::DISCLAIMER_SV`), returned on every
analysis, and asserted by the golden dataset to be present and unaltered.

## Quality target

Not the most findings — the most *useful and verifiable* findings per analysis.
Five strong opportunities beat forty hallucinated ones. The golden dataset
measures this across ten synthetic companies and currently runs at precision
1.000 with zero false positives.
