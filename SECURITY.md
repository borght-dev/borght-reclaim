# Security policy

## Reporting a vulnerability

Report security issues privately. Open the repository's **Security** tab and choose **Report a vulnerability**. Do not open a public issue.

Include what the issue is, how to reproduce it, and which version you ran.

I will answer within a few days and agree a timeline for the fix and the disclosure with you.

## What counts

Borght Reclaim deletes files. The reports that matter most are the ones where it can be made to delete something it should not:

- a path outside the scanned folder, for example through a junction or symbolic link
- anything under `Windows`, `Program Files` or `ProgramData`
- a folder that a running program still uses
- git work that exists nowhere else

A wrong rating that needs no attacker, such as a folder marked safe that should be blocked, is a normal bug. Open an issue for those.

## Out of scope

- Issues that need an attacker who already controls the local machine or the user account
- Vulnerabilities in dependencies that have their own advisory and no exploit path through Reclaim
