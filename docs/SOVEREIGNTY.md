# Māori data sovereignty in Pātaka

A pātaka is a raised storehouse. Food and taonga kept in it were protected, and those responsible for it decided what went in or out. Pātaka (the app) tries to give people and communities the same control over the kōrero they have with AI models, most of which run offshore.

This document explains how the design draws on the **Te Mana Raraunga Principles of Māori Data Sovereignty** and the **CARE Principles for Indigenous Data Governance** (Collective benefit, Authority to control, Responsibility, Ethics). It does not claim compliance or endorsement: it is a description of what the software does, so that iwi, hapū, researchers and data stewards can judge it themselves.

## Principles and what the software does

### Rangatiratanga: authority to control

- **Nothing leaves the device without a decision.** Every message bound for a model off the device goes through Tūmataiti redaction, and you can review the exact outbound text first.
- **Tapu kōrero stay home.** A kōrero marked *tapu* can only go to a model on this computer, or to infrastructure you have marked *sovereign* (for example a server run by your iwi or organisation). Pātaka decides "on this device" from the provider's address, so a remote service cannot claim it.
- **Tapu terms block.** Any message containing a term on the kaitiaki tapu list is refused outright, whatever the rest of the policy says. A tapu term cannot be allow-listed.
- **Local first.** There is no account, server, telemetry or sync. Data lives on the device, encrypted, under a key only the user holds.

### Whakapapa: provenance and context

- Each message records which provider and model handled it, and where the provider says it processes data (for example *United States*, *China*, *France (EU)*, *This device*).
- Each user message keeps a copy of exactly what left the device, so the user can see it later.
- The audit log records which kaupapa (policy) version, identified by its hash, was in force for each send.

### Whanaungatanga and kotahitanga: collective rights and interests

- **Affiliation is personal data.** Iwi, hapū, marae and whānau names identify people in small communities even without a name, so the default policy pseudonymises them rather than just flagging them.
- **Kaitiaki lists.** A data steward can supply lists of names, places, affiliations, health terms and tapu terms that apply to every kōrero. Matching ignores macrons and case (*Hēmi* = *hemi*).
- **Tūpuna and public figures.** Names that may properly be discussed in research can be allow-listed so questions stay answerable.

### Manaakitanga: care and reciprocity

- The review step shows what was found, the action taken and the placeholder used, and lets the user redact more before sending.
- Replies are restored on the device, so the user reads a natural answer and the provider never sees the original values.

### Kaitiakitanga: guardianship

- Everything is encrypted at rest. The audit log is tamper-evident and holds counts only.
- Retention can be set so kōrero are deleted automatically after a number of days.
- Any kōrero can be exported (Markdown or JSON), and everything can be deleted in one step, including the key.

## Defaults (the kaupapa)

| Category | Default action |
|---|---|
| Person, address, email, phone, URL, IP address | Pseudonymise (`[PERSON_1]`, restored in replies) |
| Date of birth or death | Generalise to the year |
| NZ government ID (IRD, NHI, passport, licence), bank account, card, other ID | Mask (not restored) |
| Passwords and API keys | Block the message |
| Affiliation (iwi, hapū, marae, whānau) | Pseudonymise |
| Health | Flag for review |
| Tapu / culturally restricted | Block the message (cannot be changed) |

These follow Tūmataiti's recall-first defaults. Policy for affiliation, health and culturally restricted content belongs to kaitiaki and the communities concerned, not to the software. The table is editable in **Settings → Tūmataiti**.

## What the software cannot do

- It cannot decide what is tapu, or who should see mātauranga. **The tool flags; people decide.**
- No detector finds everything. The Tūmataiti name model runs on the device and finds most names, addresses and affiliation. Without it, they are only redacted if they are on a kaitiaki list or added in the review step.
- The name model is a derived artefact of its training data, and its governance (who may change it, and how communities can ask for changes) sits with the Tūmataiti project. Pātaka pins one published version by hash and never updates it silently.
- Redaction reduces risk; it does not anonymise. A capable model can sometimes infer identity from context.
- Once text reaches a provider, that provider's terms and jurisdiction apply.

## References

- Te Mana Raraunga, *Principles of Māori Data Sovereignty* (2018).
- Global Indigenous Data Alliance, *CARE Principles for Indigenous Data Governance* (2019).
- Tūmataiti specification: https://github.com/woodRock/tumataiti/blob/main/SPEC.md
