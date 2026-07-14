# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through [GitHub Security Advisories](https://github.com/shuv1337/sess/security/advisories/new).
Do not open a public issue for an undisclosed vulnerability.

Include reproduction steps, affected versions, and the expected impact when possible. You should receive an acknowledgement within seven days.

## Sensitive local data

`sess` indexes coding-agent transcripts, which may contain source code, credentials, or other confidential material. Its SQLite database, Tantivy index, and optional embedding data remain local, but users are responsible for protecting the data directory and any backups.
