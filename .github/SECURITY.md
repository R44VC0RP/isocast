# Security

## Reporting a vulnerability

Please report security problems privately, not in a public issue or
discussion. Private vulnerability reporting is turned on for this
repository:

1. Open [Report a vulnerability](https://github.com/R44VC0RP/isocast/security/advisories/new)
   (the **Security** tab → **Report a vulnerability**).
2. Describe the problem, the version (`isocast --version`) and how to
   reproduce it.

Only the maintainers see the report. We aim to reply within a week, agree
on a fix and a disclosure date with you, and credit you unless you prefer
not to be named.

Examples of what counts: a way for a plugin to escape its sandbox or call
methods it wasn't granted, another local user or app reaching the engine's
control socket, a stream key or other secret ending up in a log, a bug
report, a setup file or the screen, or a crafted setup, OBS import or media
file that runs code.

## Supported versions

Isocast is in alpha. Fixes go into the latest release only.

## Bug reports and privacy

`isocast report` (coming with the app's bug reporter) writes a zip on your
Mac and never uploads it. It redacts keys, tokens, passwords and stream
URLs, but read it before you attach it to a public issue.
