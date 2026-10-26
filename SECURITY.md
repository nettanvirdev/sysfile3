# Security policy

sysfile3 keeps many users' files in one database and one bucket, so isolation bugs matter most.
Please report anything that could let one workspace read or change another's files, escape a
confined handle, bypass a quota, or open a link that should not work.

## Reporting

Use GitHub's private vulnerability reporting: the **Security** tab of this repository, then
**Report a vulnerability**. Please do not open a public issue for a security problem.

Include what you did, what happened, and the smallest reproduction you can. You will get an
answer within a week.

## Supported versions

Only the latest release receives fixes while the project is below 1.0.
