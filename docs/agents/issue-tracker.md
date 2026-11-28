# GitHub Issues

Track specs and implementation tickets in `JayTsu-sh/robinhood-rs` with the `gh` CLI. Check the target with `gh repo view --json nameWithOwner` from the repository root. Pass `--repo JayTsu-sh/robinhood-rs` on issue writes.

## Issue operations

The examples assume the repository root and the verified repository target.

| Task | Command |
| --- | --- |
| Publish a spec or ticket | `gh issue create --title "$title" --body-file "$body_file"` |
| Read an issue or ticket | `gh issue view "$number" --json number,title,body,labels,comments` |
| List open issues | `gh issue list --state open --json number,title,body,labels,comments` |
| Create a sub-issue | `gh issue create --parent "$parent" --title "$title" --body-file "$body_file"` |
| Attach an existing sub-issue | `gh issue edit "$parent" --add-sub-issue "$child"` |
| Set a blocking dependency | `gh issue edit "$child" --add-blocked-by "$blocker"` |
| Apply a triage label | `gh issue edit "$number" --add-label "$label"` |
| Comment on an issue | `gh issue comment "$number" --body-file "$body_file"` |
| Close an issue after recording the outcome | `gh issue close "$number"` |

Use native GitHub dependencies for tickets. A ticket is startable when every blocker is closed. Count its open blockers with `gh issue view "$number" --json blockedBy --jq '[.blockedBy.nodes[] | select(.state == "OPEN")] | length'`. If the installed CLI lacks the dependency flags, use `gh api --method POST "repos/JayTsu-sh/robinhood-rs/issues/$child/dependencies/blocked_by" -F "issue_id=$blocker_id"`. Set `blocker_id` to the numeric database ID from `gh api "repos/JayTsu-sh/robinhood-rs/issues/$blocker" --jq .id`. If native dependencies are unavailable, list the blockers by issue number in the ticket body.

## Pull requests as a triage surface

**PRs as a request surface: no.**

## Wayfinding operations

Create the decision map with `wayfinder:map` and link its tickets as sub-issues, each with its corresponding `wayfinder:research`, `wayfinder:prototype`, `wayfinder:grilling`, or `wayfinder:task` label. Start with the first open, unassigned ticket whose blockers are closed. Claim it with `gh issue edit "$number" --add-assignee @me`. After resolving it, record the outcome in the ticket, close it, and link the decision from the map.
