"""Opens, updates and closes the GitHub issues that track a broken adapter, manual entries that need a
look, or a failed publish. Runs in the workflow's `report` job (GITHUB_TOKEN with issues: write only)
through the preinstalled `gh` CLI; text goes through stdin, never through a shell.

One issue per failing adapter ("Driver catalog: <adapter> adapter failing"), commented on every run
while it fails and closed when the adapter works again; one issue for warnings; one for publishing.
"""

import json
import subprocess

LABEL = "driver-catalog"
PREFIX = "Driver catalog: "


def _gh(args, stdin=None, runner=subprocess.run):
    r = runner(["gh", *args], input=stdin, text=True, capture_output=True, check=False)
    if r.returncode != 0:
        raise RuntimeError(f"gh {' '.join(args[:2])} failed: {r.stderr.strip()[:300]}")
    return r.stdout


def plan(report, open_titles, publish_ok, run_url, adapters):
    """Pure decision: -> list of ("create"|"comment"|"close", title, body)."""
    actions = []
    failing = {}
    for f in report.get("failures", []):
        failing.setdefault(f["adapter"], []).append(f)
    for name in adapters:
        title = f"{PREFIX}{name} adapter failing"
        if name in failing:
            lines = [f"The `{name}` adapter failed in {run_url}.", ""]
            for f in failing[name]:
                kept = "the previously published entry was kept" if f["kept_previous"] else "no published entry exists"
                lines.append(f"- `{f['id']}`: {f['error']} ({kept}; its checked_at is not refreshed)")
            lines += ["", "Fix the adapter, or pin the version in `catalog/manual.json` through a pull request."]
            actions.append(("comment" if title in open_titles else "create", title, "\n".join(lines)))
        elif title in open_titles:
            actions.append(("close", title, f"The `{name}` adapter works again ({run_url})."))
    title = f"{PREFIX}entries need attention"
    if report.get("warnings"):
        body = "\n".join([f"From {run_url}:", ""] + [f"- {w}" for w in report["warnings"]])
        actions.append(("comment" if title in open_titles else "create", title, body))
    elif title in open_titles:
        actions.append(("close", title, f"Nothing to look at any more ({run_url})."))
    title = f"{PREFIX}publishing failed"
    if not publish_ok:
        body = f"Building, validating, signing or committing the catalog failed in {run_url}. The app keeps using the last published catalog."
        actions.append(("comment" if title in open_titles else "create", title, body))
    elif title in open_titles:
        actions.append(("close", title, f"Published again in {run_url}."))
    return actions


def sync(report, publish_ok, run_url, adapters, repo, runner=subprocess.run):
    try:
        _gh(["label", "create", LABEL, "--repo", repo, "--color", "c5def5", "--force",
             "--description", "Weekly driver catalog job"], runner=runner)
    except RuntimeError:
        pass
    listed = json.loads(_gh(["issue", "list", "--repo", repo, "--label", LABEL, "--state", "open",
                             "--limit", "100", "--json", "number,title"], runner=runner) or "[]")
    open_issues = {i["title"]: i["number"] for i in listed}
    done = []
    for action, title, body in plan(report, set(open_issues), publish_ok, run_url, adapters):
        if action == "create":
            _gh(["issue", "create", "--repo", repo, "--title", title, "--label", LABEL, "--body-file", "-"], body, runner)
        elif action == "comment":
            _gh(["issue", "comment", str(open_issues[title]), "--repo", repo, "--body-file", "-"], body, runner)
        else:
            _gh(["issue", "close", str(open_issues[title]), "--repo", repo, "--comment", body], runner=runner)
        done.append((action, title))
    return done
