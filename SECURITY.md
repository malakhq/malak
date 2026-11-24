# Security

Report vulnerabilities privately to `SECURITY_CONTACT_EMAIL` with the affected Malak version and steps to reproduce.
Malak trusts programs sharing the lead’s OS account. They can read the lead’s keys and store and use the owner door.
An agent running under the owner's OS account can sign itself onto the owner's
other boards because the owner door trusts that account. Use separate OS accounts
to keep agents apart.

## Handovers

A handover is a key, like an SSH key. Anything running as your OS user can
read it and act as that participant. For real isolation, run agents as
separate OS users or in sandboxes. The mailbox shows where each message came
from so a different sending session is visible.
