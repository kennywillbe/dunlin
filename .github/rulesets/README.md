# Repository rules, as files

Branch protection on `main` and the tag rule for releases live in GitHub
settings, where nobody can review a change to them. These files are the same
rules exported, so a change shows up in a diff.

GitHub is the source of truth, not these files. Re-export after changing
anything:

```sh
gh api repos/kennywillbe/dunlin/rulesets --jq '.[].id' | while read id; do
  gh api "repos/kennywillbe/dunlin/rulesets/$id" > .github/rulesets/release-tags.json
done
gh api repos/kennywillbe/dunlin/branches/main/protection > /tmp/protection.json
```

## What they say, and why

`main-branch-protection.json`

- **Every change arrives by pull request**, and the required checks have to
  be green. `strict` means the branch must be up to date too, so a green check
  on a stale base does not count. **Audit** is not required: it only runs when
  a Cargo file changes, and a required check that never starts blocks the merge
  forever.
- **The pull request title is a required check.** Merges are squash-only, so
  the title becomes the commit subject on `main`, and release-please reads that
  subject to pick the next version and write the changelog.
- **Linear history.** A merge commit's subject is `Merge pull request #N ...`,
  which release-please cannot parse.
- **No force pushes and no deletion.**
- **`enforce_admins` is true, the owner included.** If the owner can push
  straight to `main`, the pull request list stops being the history of the
  project. In an emergency, turn it off, do the thing, turn it back on; that
  leaves a trace.
- **Approvals required: zero.** GitHub does not let anyone approve their own
  pull request, so one required approval on a single-maintainer repository
  means nothing can merge. The checks are the gate.

`release-tags.json`

- **Only an admin can create, move or delete a `v*` tag.** A tag publishes
  binaries and a container image, so it belongs to whoever owns the release.

  release-please tags with `RELEASE_PLEASE_TOKEN`, a fine-grained token owned
  by an admin, and that is what gets past this rule. The Actions app cannot be
  given a bypass on a personal repository, and GitHub does not start a workflow
  from a tag its own `GITHUB_TOKEN` pushed, so a tag made with it would publish
  nothing.
