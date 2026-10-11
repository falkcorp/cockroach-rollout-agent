### Added

#### Pull request template with agent instructions

`.github/pull_request_template.md` gives every PR the same sections: summary,
changes, invariants touched, test plan, risk and a checklist.

It also carries HTML-comment instructions, invisible once the PR renders.
They spell out, for AI agents and humans:

- the commit-title format and file-header rules;
- the changelog and TODO fragment workflow;
- the local gates to run before opening;
- the rollout and web-UI invariants a change must preserve;
- how to merge.
