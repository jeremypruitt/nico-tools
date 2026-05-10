# Triage Labels

The skills speak in terms of five canonical triage roles. This file maps those roles to the actual label strings used in this repo's issue tracker.

| Label in mattpocock/skills | Label in our tracker | Meaning                                  |
| -------------------------- | -------------------- | ---------------------------------------- |
| `needs-triage`             | `needs-triage`       | Maintainer needs to evaluate this issue  |
| `needs-info`               | `needs-info`         | Waiting on reporter for more information |
| `ready-for-agent`          | `ready-for-agent`    | Fully specified, ready for an AFK agent  |
| `ready-for-human`          | `ready-for-human`    | Requires human implementation            |
| `wontfix`                  | `wontfix`            | Will not be actioned                     |

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), use the corresponding label string from this table.

Edit the right-hand column to match whatever vocabulary you actually use.

## Priority bands

Five additional labels carry priority — `crit` / `top` / `high` / `med` / `low`. **These are derived from the Score project field by the project-automation workflow; never set them by hand.** See `issue-tracker.md` §"Priority scoring" for the full convention. Summary mapping:

| Score | Label | Action                          |
|------:|-------|---------------------------------|
| 50-100 | `crit` | Stop what you are doing         |
| 25-49  | `top`  | Current or next sprint          |
| 13-24  | `high` | Schedule in a future sprint     |
| 5-12   | `med`  | Only if nothing better exists   |
| 1-4    | `low`  | Backlog or kill                 |
