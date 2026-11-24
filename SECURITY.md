# Security

Report vulnerabilities privately through GitHub: open the repository's Security tab and choose "Report a vulnerability" (https://github.com/malakhq/malak/security/advisories/new). Include the affected Malak version and steps to reproduce. Please do not open a public issue.
Malak trusts programs sharing the lead’s OS account. They can read the lead’s keys and store and use the owner door.
An agent running under the owner's OS account can sign itself onto the owner's
other boards because the owner door trusts that account. Use separate OS accounts
to keep agents apart.

## Handovers

A handover works like an SSH key. Keep it private. On local Unix sockets,
Malak binds handovers issued by the worker, adoption, guest and lead recipes
to one process ID and start time. The session's commands and hooks can use
the handover. A different session is refused at attachment, and the board
records the refusal in the participant's claim history.

The binding rules cover accidental reuse. A program on the same OS account
can still read the owner key or interfere with the claimed process. Use
separate OS users or sandboxes for isolation. Transports without a calling
process rely on their existing source checks. Bare `malak enrol` and explicit
`--unbound` enrolments keep the original key-based access rules.
