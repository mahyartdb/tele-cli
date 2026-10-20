# Security Policy

Tele drives real Telegram user accounts. A leaked session file hands the whole account to whoever reads it, so security reports are taken seriously and handled privately.

## Reporting a vulnerability

**Do not open a public issue for a security problem.**

Use GitHub's private reporting: open the repo's **Security** tab → **Report a vulnerability**. That opens a private advisory visible only to the maintainer until a fix is ready.

If that path is unavailable, mail the maintainer at `mahyar.drama1313@gmail.com` (the same address the npm package publishes under) with:

- what the issue is and which command triggers it
- the version (`tele --version`) and platform
- reproduction steps
- the impact you expect

Please do not include real session files, `.env` contents, phone numbers, 2FA passwords, or login codes in a report. Redact first — see [docs/security.md](docs/security.md#known-exposures).

## What to expect

- Acknowledgement within a few days.
- A fix and a release for confirmed issues; credit in the changelog unless you prefer otherwise.
- No bounty is offered.

## Threat model

The full threat model, trust boundaries, and deliberate exposures live in [docs/security.md](docs/security.md). Read it before assessing a report — several behaviors that look like bugs are documented, intentional design:

- `contact list` and `takeout export` print or write phone numbers unless you pass `--redact-phones`.
- `tele raw` runs at full account power and requires an explicit `--allow-raw` ack.
- The QR login fallback prints the `tg://login?token=…` URI to an interactive stderr.

## What is not a vulnerability

- Your own session file, `.env`, or `config.toml` being readable by another local user — set the permissions yourself; `tele` tightens `.env` on load but cannot fix a permissive filesystem or umask.
- Telegram-side behavior (flood waits, spam checks, account limits) that `tele` surfaces as errors.
- Output that contains your own message content, phone numbers, or invite links.
