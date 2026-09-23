# Security Policy

## Supported versions

Only the latest release gets fixes.

## Reporting a vulnerability

Please do not open a public issue for a security problem.

Use GitHub's [private vulnerability reporting](https://github.com/kennywillbe/dunlin/security/advisories/new).
Include:

- the version (`dunlin --version`) and how you run it (binary, container)
- what an attacker can do, and the smallest reproduction you have
- logs or config, with secrets removed

You should get a reply within 5 working days. Confirmed issues get a fix or a
plan within 30 days, and you are credited in the release notes unless you
would rather not be.

## Scope

In scope: login and session handling, CSRF, the login rate limit, anything
that lets an anonymous visitor change state or read pages hidden by
`protect_read`, heartbeat token checks, secrets showing up in logs, pages or
notifications, and ways to make dunlin reach hosts it was not configured to
check.

Out of scope: attacks that need write access to the config file, the data
directory, the Docker socket or the host itself, and denial of service from a
deliberately bad config.
