# Dependency updates

Install the [Mend Renovate GitHub App](https://github.com/apps/renovate) and grant
access only to this repository. Merge the configuration PR before onboarding.
The JSON file alone does not install or activate the App. No Renovate token or
GCP credential is stored in the repository, and no privileged scheduled workflow
is added to run Renovate.

## Policy

- Wait at least seven days from the candidate version's release timestamp.
  Renovate's default release-age buffer may add a small additional delay.
- If a release timestamp is unavailable, hold the update rather than bypassing
  the wait. Review held candidates in the Dependency Dashboard.
- Disable automerge for all updates, including vulnerability fixes. CI and
  administrator review are still required; elapsed time is not a security guarantee.
- Major updates need Dashboard approval before a PR is raised. Other breaking
  changes, including 0.x Rust crate minor updates, still require careful review.
- Track Cargo dependencies, the Rust toolchain file, GitHub Actions, Terraform
  requirements/providers, Dockerfiles, and the Terraform CLI version in CI.
- Update lock files with selected dependency updates, but do not run blanket
  lock-file maintenance. Transitive lock-file changes may include newer releases;
  review the complete diff, not only the direct dependency's age.
- Keep the Ubuntu image on 24.04 until a coordinated runner/runtime migration.
  Existing tags can still be mutable; this policy does not pin their digests.
- Limit normal open PRs to three and new PRs to two per hour. Security alerts
  can bypass Renovate's normal rate limits, but remain manually reviewed.

Vulnerability-fix updates also use the seven-day age setting. If an urgent fix
cannot wait, an administrator must explicitly review and apply it manually;
do not silently enable automerge or reduce the cooldown for all updates.

PR titles, introductory text, and table headings use Japanese. Upstream release
notes and some Renovate-generated messages retain their original language.
Python dependencies are not managed while the Python implementation is being
retired. Renovate does not create application release tags or deploy to GCP.