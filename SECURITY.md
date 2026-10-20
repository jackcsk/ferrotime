# Security policy

Ferrotime is used to open files collected during investigations, which may be crafted by an attacker. Bugs that let a malicious file crash or hang the application, exhaust memory, or do anything beyond being displayed are treated as security issues.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's **[Report a vulnerability](https://github.com/jackcsk/ferrotime/security/advisories/new)** form, not in public issues. Include the Ferrotime version, your platform, and a minimal file or steps that reproduce the problem (synthetic data only, never real case data).

You can expect an acknowledgement within a week. Once a fix is available, the advisory is published with credit to the reporter unless you ask otherwise.

## Supported versions

Security fixes are made on the latest release only.

## Scope notes

- Ferrotime only reads the files you open; it never modifies them and makes no network connections.
- Session files (`.ferrotime`, `.tle_sess`) contain file paths. Opening a session opens the files it lists that exist on your machine, read-only.
