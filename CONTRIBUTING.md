# Contributing to Malak

## Build the workspace

Use macOS or Linux with Git, Rust 1.95 or later with rustfmt and Clippy,
a C compiler for bundled SQLite, and Python 3.11 or later for recipes.
Set `MALAK_SOURCE` to the clone URL or local checkout path you received.

```sh
git clone "$MALAK_SOURCE" malak
cd malak
git switch -c my-change
cargo build --locked --workspace
```

## Add a test

Put the test next to the closest existing tests for the behavior you are
changing. Give it a name that states the expected behavior and use existing
fixture helpers where possible. For example, add a name validation case in
`crates/malak-layers/tests/names.rs` and run that integration test directly.

```sh
cargo test -p malak-layers --test names
```

## Change a recipe

Edit the recipe under `crates/malak/recipes/`. The `recipes/` path at the
repository root points there. Check [the recipe guide](docs/RECIPES.md) when
changing arguments, output, or setup, and update the guide when the documented
behavior changes. Add a case to `crates/malak/tests/recipes.rs` when the
change needs coverage through the CLI. Build and run those tests with Cargo.

```sh
cargo test -p malak --test recipes
```

When changing the observe hook, also replay its payloads and run its local
board tests using the CLI you just built. Use a short checkout path under
directories owned by you or root and writable only by their owners.
A directory under your home is a typical choice.

```sh
(
    umask 077
    export PATH="$PWD/target/debug:$PATH"
    python3 -B -m unittest discover -s recipes/tests -v
)
```

Changes to the release script can be checked in isolated Git repositories.

```sh
python3 -B -m unittest discover -s scripts/tests -v
```

## Propose a behavior change

Open a discussion or issue before changing documented behavior. Name the
affected command or interface, explain what should change and why, and
give an example with a testable expected result. Identify any other
behavior that depends on the existing rule. Wait for the project owner to
accept the change and document the updated behavior before implementing it.
Link the decision in your pull request.

## How changes land

1. Every change starts as an issue. Bugs, features and docs have their own forms. Security problems are reported privately (Security tab).
2. The maintainer labels it: one kind (`bug`, `enhancement`, `docs`, `ci`, `flaky-test`) and an area (`cli`, `mailbox`, `recipes`, `board`, `agents`).
3. The fix arrives as a pull request whose description says `Fixes #N`.
4. CI runs `scripts/check` on Linux. `main` only accepts pull requests with a green check. Nobody pushes to it directly.
5. Merging closes the issue. Releases are tagged from `main`, and their notes list the merged pull requests by label.

## Sync fixes and cut releases

Maintainers open one public pull request per lane after merging it into private
`main`. Use a clean public clone and refresh local `main` and `origin/main` to
the same public commit. Run the maintainer commands from the private checkout.

```sh
internal/lane-merge NAME --sync
```

Include `Fixes #N` in the lane's commit messages. The exporter scans and checks
the filtered lane changes on top of public `main` in a temporary checkout. The
public pull request describes the changes and checks, links the fixed issues,
and inherits their labels. For a batch of pending changes, run
`internal/sync-pr --fixes 1 --fixes 2` with all the issue numbers.

After the required check goes green, run `internal/merge-pr N` with the public
pull request number. It uses the pull request title as the squash commit
subject, deletes the branch, and verifies the commit's GitHub no-reply author.

When it is time to release, land the version bump through the same sync
process. From the private checkout, run `internal/cut-release vX.Y.Z`. It
checks the workspace version and successful push-triggered CI for the exact
public `main` commit, pushes the tag, and creates the GitHub release with
generated notes. It then publishes the workspace crates to crates.io from a
temporary worktree at that tag. Only `cut-release` publishes crates. CI and
the sync and merge commands leave crates.io untouched.

Use `internal/cut-release --no-publish vX.Y.Z` for a GitHub-only release.
Use `internal/cut-release --publish-only vX.Y.Z` to resume crate publication
for an existing GitHub release. It skips versions already on crates.io.

## Commit and open a pull request

Keep each commit focused and passing. Use an imperative subject of up to 60
characters, such as `Clarify worker report usage`. Add a short body when the
reason would be hard to infer from the diff. From the checkout root, run the
formatting, Clippy, workspace test and documentation checks before each commit.

```sh
scripts/check
git diff
```

Stage only the files that belong in the commit.

If Git needs your identity, set `user.name` and `user.email` with `git config`
in your clone before committing.

In the pull request, describe what behavior changed and why, link the issue
or project owner's decision when applicable, and list the checks you ran and
their results. Include a regression test for a bug fix and an updated example
when a command or recipe changes how people use Malak.
