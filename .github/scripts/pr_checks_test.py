#!/usr/bin/env python3
"""Tests for pr_checks.py: python3 .github/scripts/pr_checks_test.py"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pr_checks  # noqa: E402

TEMPLATE = open(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "PULL_REQUEST_TEMPLATE.md"),
    encoding="utf-8",
).read()

UI = ["crates/isocast-ui/ui/app.slint", "README.md"]
ENGINE = ["crates/isocast-engine/src/lib.rs"]

VERIFIED = """## How it was verified

cargo test: 214 passed. Recorded 10 s, ffprobe: hevc 1920x1080 60/1, 600 frames.
"""


def body(verified=VERIFIED, extra=""):
    return f"## What and why\n\nFixes the meter.\n\n{verified}\n## Screenshots\n\n{extra}\n"


class PrChecks(unittest.TestCase):
    def check(self, text, files):
        return pr_checks.problems(text, files)

    def test_engine_change_with_evidence_passes(self):
        self.assertEqual(self.check(body(), ENGINE), [])

    def test_ui_change_needs_an_image(self):
        found = self.check(body(), UI)
        self.assertEqual(len(found), 1)
        self.assertIn("screenshot", found[0])

    def test_ui_paths(self):
        self.assertTrue(pr_checks.touches_ui(["crates/isocast-app/ui/app.js"]))
        self.assertFalse(pr_checks.touches_ui(["crates/isocast-app/src/main.rs"]))
        self.assertFalse(pr_checks.touches_ui(["docs/crates/isocast-ui/x.md"]))

    def test_markdown_image_counts(self):
        self.assertEqual(self.check(body(extra="![before](https://example.com/a.png)"), UI), [])

    def test_html_image_counts(self):
        self.assertEqual(self.check(body(extra='<img width="600" src="https://example.com/a.png">'), UI), [])

    def test_uploaded_attachment_counts(self):
        url = "https://github.com/user-attachments/assets/0f1e2d3c-4b5a-6978-8a9b-0c1d2e3f4a5b"
        self.assertEqual(self.check(body(extra=url), UI), [])

    def test_image_inside_a_comment_does_not_count(self):
        self.assertEqual(len(self.check(body(extra="<!-- ![x](https://example.com/a.png) -->"), UI)), 1)

    def test_link_is_not_an_image(self):
        self.assertEqual(len(self.check(body(extra="[before](https://example.com/a.png)"), UI)), 1)

    def test_empty_verification_fails(self):
        found = self.check(body(verified="## How it was verified\n\n<!-- What you ran -->\n\n"), ENGINE)
        self.assertEqual(len(found), 1)
        self.assertIn("empty", found[0])

    def test_bare_checkboxes_are_empty(self):
        found = self.check(body(verified="## How it was verified\n\n- [ ] \n- \n"), ENGINE)
        self.assertEqual(len(found), 1)

    def test_missing_section_fails(self):
        found = self.check("Fixes the meter.", ENGINE)
        self.assertEqual(len(found), 1)
        self.assertIn("no \"How it was verified\"", found[0])

    def test_empty_body_fails(self):
        self.assertEqual(len(self.check(None, UI)), 2)

    def test_unfilled_template_fails(self):
        found = self.check(TEMPLATE, UI)
        self.assertEqual(len(found), 2, found)

    def test_filled_template_passes(self):
        filled = TEMPLATE.replace(
            "<!-- Commands you ran and what they printed",
            "cargo test: ok. isocast-ui --snapshot before/after below.\n<!-- Commands you ran and what they printed",
        ).replace(
            "<!-- Required for any change to the app's interface",
            "![after](https://github.com/user-attachments/assets/abc)\n<!-- Required for any change to the app's interface",
        )
        self.assertEqual(self.check(filled, UI), [])

    def test_heading_level_and_case(self):
        text = "### HOW IT WAS VERIFIED ###\nran cargo test\n"
        self.assertEqual(self.check(text, ENGINE), [])


if __name__ == "__main__":
    unittest.main()
