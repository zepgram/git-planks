---
name: git-planks
description: Analyzes who owns the code still alive across one repository or a whole stack (GitHub, GitLab, Bitbucket, any git remote) with the git-planks CLI, and produces a deterministic interactive report. Use when asked about code ownership, surviving lines, who wrote the code, code survival, bus factor, or how a codebase and its team evolved.
---

# git-planks

Requires the `git-planks` binary and `git`. Reports name real people: keep them where the user decides.

1. **Confirm the scope.** `git-planks repos <sources>` lists what would be analyzed. Sources: `github:owner`, `gitlab:group/subgroup`, `bitbucket:workspace`, `git@host:org/repo.git`, a folder, `@file`. Prefix `group=` to split the report into scopes.
2. **Analyze, in the background.** `git-planks analyze <sources> --json --out planks-report`. A first run on a large stack takes minutes; later runs reuse the cache. Any failure names the repository: report it rather than dropping it with `--keep-going`, unless the user accepts a partial report.
3. **Audit before trusting a figure.** `git-planks audit planks-report`. A jump credited to one person is usually an import or generated code: exclude its paths or set `import_subject` in `planks.toml`, then run step 2 again. Read `planks-report/identities.json` for wrong merges and fix them with `[[person]]`. See `examples/planks.toml`.
4. **Prove it.** `git-planks verify planks-report` blames everything again, requires identical files and checks every line has one author. It takes as long as a first analysis: run it in the background.
5. **Report** the totals and top contributors from the JSON summary, the steps found in step 3, and the limits: a line that moved can change author, a commit written with an AI stays under the name of whoever pushed it, and lines describe code, never a person's value.
