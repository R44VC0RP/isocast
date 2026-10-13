#!/usr/bin/env python3
"""Checks a pull request's description against CONTRIBUTING.md's evidence rules.

    pr_checks.py BODY_FILE CHANGED_FILES_FILE

BODY_FILE is the pull request's description; CHANGED_FILES_FILE has one
changed path per line. Exits 1 with a message for each rule broken:

- a change to the app's interface (crates/isocast-ui/** or
  crates/isocast-app/ui/**) needs at least one image in the description;
- the template's "How it was verified" section can't be empty.

Run by .github/workflows/pr-checks.yml; tested by pr_checks_test.py.
"""

import re
import sys

UI_PATHS = ("crates/isocast-ui/", "crates/isocast-app/ui/")
VERIFIED_HEADING = "how it was verified"

IMAGE_PATTERNS = (
    re.compile(r"!\[[^\]]*\]\(\s*\S+\s*\)"),  # markdown image
    re.compile(r"<img\b[^>]*\bsrc\s*=", re.IGNORECASE),  # HTML image
    re.compile(r"https://github\.com/user-attachments/(assets|files)/\S+"),  # drag-and-drop upload
)


def strip_comments(text):
    return re.sub(r"<!--.*?-->", "", text, flags=re.DOTALL)


def touches_ui(files):
    return any(path.startswith(UI_PATHS) for path in files)


def has_image(body):
    body = strip_comments(body)
    return any(pattern.search(body) for pattern in IMAGE_PATTERNS)


def section(body, heading):
    """The text under a `## heading` (any level), up to the next heading, or None."""
    lines = body.splitlines()
    for i, line in enumerate(lines):
        match = re.match(r"^\s{0,3}#{1,6}\s+(.*?)\s*#*\s*$", line)
        if match and match.group(1).strip().lower().startswith(heading):
            rest = []
            for following in lines[i + 1:]:
                if re.match(r"^\s{0,3}#{1,6}\s", following):
                    break
                rest.append(following)
            return "\n".join(rest)
    return None


def is_empty(text):
    text = strip_comments(text)
    # Unticked or ticked checkbox lines with nothing after them, bare list
    # markers and blank lines don't count as evidence.
    meaningful = [
        line for line in text.splitlines()
        if re.sub(r"^\s*([-*+]|\d+\.)?\s*(\[[ xX]\])?\s*", "", line).strip()
    ]
    return not meaningful


def problems(body, files):
    body = body or ""
    found = []
    if touches_ui(files) and not has_image(body):
        found.append(
            "This pull request changes the app's interface "
            "(crates/isocast-ui or crates/isocast-app/ui) but its description has no screenshot. "
            "Add before/after images (scripts/ui-screenshots.sh makes them), then edit the description."
        )
    verified = section(body, VERIFIED_HEADING)
    if verified is None:
        found.append(
            'The description has no "How it was verified" section. '
            "Use the pull request template and say what you ran and what you saw."
        )
    elif is_empty(verified):
        found.append(
            'The "How it was verified" section is empty. '
            "List the commands you (or your agent) ran and their output: tests, ffprobe, screenshots, bench.py numbers."
        )
    return found


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    with open(sys.argv[1], encoding="utf-8") as f:
        body = f.read()
    with open(sys.argv[2], encoding="utf-8") as f:
        files = [line.strip() for line in f if line.strip()]
    found = problems(body, files)
    for problem in found:
        print(f"::error::{problem}")
    if found:
        print("See CONTRIBUTING.md for what a pull request needs.")
        sys.exit(1)
    print(f"Pull request description OK ({len(files)} changed files, UI change: {touches_ui(files)}).")


if __name__ == "__main__":
    main()
