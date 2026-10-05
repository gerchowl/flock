from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from scripts.changelog import (
    archived_releases_from_current_manifest,
    build_latest_json,
    build_no_release_manifest,
    build_release_plan,
    bump_version,
    canonicalize_manifest,
    ChangelogError,
    DEFAULT_PRODUCT_ANNOUNCEMENT_PATH,
    DEFAULT_RELEASE_REPO,
    DEFAULT_LATEST_JSON_PATH,
    default_release_assets,
    ensure_current_release_assets_are_mirrored,
    ensure_manifest_assets_belong_to_repo,
    ensure_manifest_is_outdated,
    ensure_manifest_matches_expected,
    ensure_version_is_releasable,
    extract_section_body,
    foreign_release_entries,
    infer_protocol_from_notes,
    load_json,
    load_product_announcement,
    manifest_from_release_payload,
    NO_STABLE_RELEASE_VERSION,
    prepare_release,
    read_protocol_version,
    resolve_release_base,
)
from scripts.test_support import commit, make_repo, tag

FOREIGN_REPO = "example.invalid/another-project"


class ChangelogScriptTests(unittest.TestCase):
    def test_prepare_release_moves_unreleased_into_versioned_section(self) -> None:
        original = """# Changelog\n\n## Unreleased\n\n### Fixed\n- Smoothed Claude flapping.\n\n## [0.1.0] - 2026-03-27\n\n### Added\n- Initial release.\n"""

        updated = prepare_release(original, "0.1.1", "2026-03-28")

        self.assertIn("## Unreleased\n\n## [0.1.1] - 2026-03-28", updated)
        self.assertIn("### Fixed\n- Smoothed Claude flapping.", updated)
        self.assertIn("## [0.1.0] - 2026-03-27", updated)

    def test_prepare_release_accepts_bracketed_unreleased_heading(self) -> None:
        original = """# Changelog\n\n## [Unreleased]\n\n### Added\n- Added sounds.\n"""

        updated = prepare_release(original, "0.1.1", "2026-03-28")

        self.assertIn("## Unreleased\n\n## [0.1.1] - 2026-03-28", updated)
        self.assertIn("### Added\n- Added sounds.", updated)

    def test_extract_section_body_returns_requested_version_only(self) -> None:
        changelog = """# Changelog\n\n## Unreleased\n\n## [0.1.1] - 2026-03-28\n\n### Fixed\n- Smoothed Claude flapping.\n\n## [0.1.0] - 2026-03-27\n\n### Added\n- Initial release.\n"""

        body = extract_section_body(changelog, "0.1.1")

        self.assertEqual(body, "### Fixed\n- Smoothed Claude flapping.\n")

    def test_build_latest_json_trims_notes(self) -> None:
        manifest = json.loads(
            build_latest_json(
                "0.1.1",
                "\n### Fixed\n- One\n\n",
                default_release_assets("0.1.1"),
            )
        )

        self.assertEqual(manifest["protocol"], read_protocol_version())
        self.assertEqual(manifest["notes"], "### Fixed\n- One")

    def test_build_latest_json_embeds_notes_and_release_assets(self) -> None:
        manifest = json.loads(
            build_latest_json(
                "v0.1.1",
                "### Fixed\n- Smoothed Claude flapping.\n",
                default_release_assets("0.1.1"),
            )
        )

        self.assertEqual(manifest["version"], "0.1.1")
        self.assertEqual(manifest["protocol"], read_protocol_version())
        self.assertEqual(manifest["notes"], "### Fixed\n- Smoothed Claude flapping.")
        self.assertEqual(
            manifest["assets"],
            {
                "linux-x86_64": "https://github.com/gerchowl/flock/releases/download/v0.1.1/flock-linux-x86_64",
                "linux-aarch64": "https://github.com/gerchowl/flock/releases/download/v0.1.1/flock-linux-aarch64",
                "macos-x86_64": "https://github.com/gerchowl/flock/releases/download/v0.1.1/flock-macos-x86_64",
                "macos-aarch64": "https://github.com/gerchowl/flock/releases/download/v0.1.1/flock-macos-aarch64",
            },
        )
        self.assertEqual(manifest["releases"]["0.1.1"]["assets"], manifest["assets"])

    def test_build_latest_json_embeds_product_announcement(self) -> None:
        manifest = json.loads(
            build_latest_json(
                "0.1.1",
                "### Fixed\n- One",
                default_release_assets("0.1.1"),
                announcement={"id": "keybinding-v2", "title": "Keybind Refactor", "body": "body"},
            )
        )

        self.assertEqual(
            manifest["announcement"],
            {"id": "keybinding-v2", "title": "Keybind Refactor", "body": "body"},
        )
        self.assertEqual(
            manifest["releases"]["0.1.1"]["announcement"],
            {"id": "keybinding-v2", "title": "Keybind Refactor", "body": "body"},
        )

    def test_build_latest_json_preserves_previous_release_metadata(self) -> None:
        manifest = json.loads(
            build_latest_json(
                "0.1.2",
                "### Fixed\n- Two",
                default_release_assets("0.1.2"),
                releases={"0.1.1": {"notes": "### Fixed\n- One"}},
            )
        )

        self.assertEqual(list(manifest["releases"]), ["0.1.2", "0.1.1"])
        self.assertEqual(manifest["releases"]["0.1.2"]["notes"], "### Fixed\n- Two")
        self.assertEqual(manifest["releases"]["0.1.2"]["protocol"], read_protocol_version())
        self.assertEqual(manifest["releases"]["0.1.2"]["assets"], default_release_assets("0.1.2"))
        self.assertEqual(manifest["releases"]["0.1.1"]["notes"], "### Fixed\n- One")
        self.assertEqual(manifest["releases"]["0.1.1"]["assets"], default_release_assets("0.1.1"))

    def test_build_latest_json_accepts_release_metadata_assets(self) -> None:
        assets = default_release_assets("0.1.1")
        manifest = json.loads(
            build_latest_json(
                "0.1.2",
                "### Fixed\n- Two",
                default_release_assets("0.1.2"),
                releases={"0.1.1": {"notes": "### Fixed\n- One", "assets": assets}},
            )
        )

        self.assertEqual(manifest["releases"]["0.1.1"]["assets"], assets)

    def test_build_latest_json_preserves_release_metadata_protocol(self) -> None:
        manifest = json.loads(
            build_latest_json(
                "0.1.2",
                "### Fixed\n- Two",
                default_release_assets("0.1.2"),
                releases={"0.1.1": {"notes": "### Fixed\n- One", "protocol": 7}},
            )
        )

        self.assertEqual(manifest["releases"]["0.1.1"]["protocol"], 7)

    def test_build_latest_json_infers_release_metadata_protocol_from_notes(self) -> None:
        manifest = json.loads(
            build_latest_json(
                "0.1.2",
                "### Fixed\n- Two",
                default_release_assets("0.1.2"),
                releases={
                    "0.1.1": {
                        "notes": "### Breaking Changes\n- The client/server protocol is now version 7."
                    }
                },
            )
        )

        self.assertEqual(manifest["releases"]["0.1.1"]["protocol"], 7)

    def test_archived_releases_from_current_manifest_seeds_legacy_root(self) -> None:
        releases = archived_releases_from_current_manifest(
            {
                "version": "0.1.1",
                "protocol": 3,
                "notes": "### Fixed\n- One",
                "announcement": {
                    "id": "one",
                    "title": "One",
                    "body": "body",
                },
            }
        )

        self.assertEqual(
            releases,
            {
                "0.1.1": {
                    "notes": "### Fixed\n- One",
                    "protocol": 3,
                    "assets": default_release_assets("0.1.1"),
                    "announcement": {
                        "id": "one",
                        "title": "One",
                        "body": "body",
                    },
                }
            },
        )

    def test_archived_releases_from_current_manifest_prefers_root_for_current_version(self) -> None:
        releases = archived_releases_from_current_manifest(
            {
                "version": "0.1.2",
                "protocol": read_protocol_version(),
                "notes": "### Fixed\n- Root",
                "releases": {
                    "0.1.2": {"notes": "### Fixed\n- Stale"},
                    "0.1.1": {"notes": "### Fixed\n- One"},
                },
            }
        )

        self.assertEqual(releases["0.1.2"]["notes"], "### Fixed\n- Root")
        self.assertEqual(releases["0.1.1"]["notes"], "### Fixed\n- One")
        self.assertEqual(releases["0.1.2"]["protocol"], read_protocol_version())
        self.assertEqual(releases["0.1.2"]["assets"], default_release_assets("0.1.2"))
        self.assertEqual(releases["0.1.1"]["assets"], default_release_assets("0.1.1"))

    def test_infer_protocol_from_notes(self) -> None:
        self.assertEqual(
            infer_protocol_from_notes("The client/server protocol is now version 10."),
            10,
        )
        self.assertEqual(
            infer_protocol_from_notes("The client/server protocol version 9."),
            9,
        )
        self.assertIsNone(infer_protocol_from_notes("No wire changes."))

    def write_temp_json(self, content: str) -> Path:
        tmp = tempfile.NamedTemporaryFile("w", delete=False, encoding="utf-8")
        with tmp:
            tmp.write(content)
        return Path(tmp.name)

    def test_checked_in_product_announcement_is_valid_or_null(self) -> None:
        self.assertTrue(DEFAULT_PRODUCT_ANNOUNCEMENT_PATH.is_file())
        load_product_announcement(DEFAULT_PRODUCT_ANNOUNCEMENT_PATH)

    def test_load_product_announcement_accepts_null(self) -> None:
        path = self.write_temp_json("null\n")
        try:
            self.assertIsNone(load_product_announcement(path))
        finally:
            path.unlink(missing_ok=True)

    def test_load_product_announcement_accepts_valid_object(self) -> None:
        path = self.write_temp_json(
            json.dumps({"id": "keybinding-v2", "title": "Keybind Refactor", "body": "Body"})
        )
        try:
            self.assertEqual(
                load_product_announcement(path),
                {"id": "keybinding-v2", "title": "Keybind Refactor", "body": "Body"},
            )
        finally:
            path.unlink(missing_ok=True)

    def test_load_product_announcement_rejects_missing_file(self) -> None:
        path = Path(tempfile.gettempdir()) / "flock-missing-product-announcement.json"
        path.unlink(missing_ok=True)
        with self.assertRaisesRegex(ChangelogError, "file not found"):
            load_product_announcement(path)

    def test_load_product_announcement_rejects_missing_empty_or_extra_fields(self) -> None:
        cases = [
            ({"id": "keybinding-v2", "title": "Keybind Refactor"}, "body"),
            ({"id": "", "title": "Keybind Refactor", "body": "Body"}, "id"),
            ({"id": "keybinding-v2", "title": "Keybind Refactor", "body": "Body", "cta": "x"}, "unsupported"),
        ]
        for payload, expected in cases:
            path = self.write_temp_json(json.dumps(payload))
            try:
                with self.assertRaisesRegex(ChangelogError, expected):
                    load_product_announcement(path)
            finally:
                path.unlink(missing_ok=True)

    def test_load_product_announcement_rejects_invalid_id(self) -> None:
        path = self.write_temp_json(
            json.dumps({"id": "Keybinding V2", "title": "Keybind Refactor", "body": "Body"})
        )
        try:
            with self.assertRaisesRegex(ChangelogError, "invalid id"):
                load_product_announcement(path)
        finally:
            path.unlink(missing_ok=True)

    def test_manifest_from_release_payload_uses_release_body_and_asset_urls(self) -> None:
        manifest = manifest_from_release_payload(
            {
                "tagName": "v0.1.1",
                "isDraft": False,
                "isPrerelease": False,
                "body": "### Fixed\n- One\n",
                "assets": [
                    {"name": "flock-linux-x86_64", "url": "https://example.com/linux-x86_64"},
                    {"name": "flock-linux-aarch64", "url": "https://example.com/linux-aarch64"},
                    {"name": "flock-macos-x86_64", "url": "https://example.com/macos-x86_64"},
                    {"name": "flock-macos-aarch64", "url": "https://example.com/macos-aarch64"},
                ],
            },
            "0.1.1",
        )

        self.assertEqual(
            manifest,
            {
                "version": "0.1.1",
                "protocol": read_protocol_version(),
                "notes": "### Fixed\n- One",
                "assets": {
                    "linux-x86_64": "https://example.com/linux-x86_64",
                    "linux-aarch64": "https://example.com/linux-aarch64",
                    "macos-x86_64": "https://example.com/macos-x86_64",
                    "macos-aarch64": "https://example.com/macos-aarch64",
                },
            },
        )

    def test_manifest_from_release_payload_uses_explicit_protocol(self) -> None:
        manifest = manifest_from_release_payload(
            {
                "tagName": "v0.1.1",
                "isDraft": False,
                "isPrerelease": False,
                "body": "### Fixed\n- One\n",
                "assets": [
                    {"name": "flock-linux-x86_64", "url": "https://example.com/linux-x86_64"},
                    {"name": "flock-linux-aarch64", "url": "https://example.com/linux-aarch64"},
                    {"name": "flock-macos-x86_64", "url": "https://example.com/macos-x86_64"},
                    {"name": "flock-macos-aarch64", "url": "https://example.com/macos-aarch64"},
                ],
            },
            "0.1.1",
            protocol=42,
        )

        self.assertEqual(manifest["protocol"], 42)

    def test_manifest_from_release_payload_rejects_missing_asset(self) -> None:
        with self.assertRaisesRegex(ChangelogError, "missing asset flock-macos-aarch64"):
            manifest_from_release_payload(
                {
                    "tagName": "v0.1.1",
                    "isDraft": False,
                    "isPrerelease": False,
                    "body": "### Fixed\n- One\n",
                    "assets": [
                        {"name": "flock-linux-x86_64", "url": "https://example.com/linux-x86_64"},
                        {"name": "flock-linux-aarch64", "url": "https://example.com/linux-aarch64"},
                        {"name": "flock-macos-x86_64", "url": "https://example.com/macos-x86_64"},
                    ],
                },
                "0.1.1",
            )

    def test_ensure_manifest_is_outdated_rejects_same_or_newer_version(self) -> None:
        with self.assertRaisesRegex(ChangelogError, "already at v0.1.1"):
            ensure_manifest_is_outdated({"version": "0.1.1"}, "0.1.1")

        with self.assertRaisesRegex(ChangelogError, "already at v0.1.2"):
            ensure_manifest_is_outdated({"version": "0.1.2"}, "0.1.1")

    def test_ensure_manifest_is_outdated_allows_older_version(self) -> None:
        ensure_manifest_is_outdated({"version": "0.1.0"}, "0.1.1")

    def test_canonicalize_manifest_requires_all_asset_targets(self) -> None:
        with self.assertRaisesRegex(ChangelogError, "missing asset URL for macos-aarch64"):
            canonicalize_manifest(
                {
                    "version": "0.1.1",
                    "protocol": read_protocol_version(),
                    "notes": "### Fixed\n- One",
                    "assets": {
                        "linux-x86_64": "https://example.com/linux-x86_64",
                        "linux-aarch64": "https://example.com/linux-aarch64",
                        "macos-x86_64": "https://example.com/macos-x86_64",
                    },
                },
                "test manifest",
            )

    def test_ensure_manifest_matches_expected_normalizes_whitespace(self) -> None:
        actual = {
            "version": "v0.1.1",
            "protocol": read_protocol_version(),
            "notes": "\n### Fixed\n- One\n",
            "assets": {
                "linux-x86_64": " https://example.com/linux-x86_64 ",
                "linux-aarch64": "https://example.com/linux-aarch64",
                "macos-x86_64": "https://example.com/macos-x86_64",
                "macos-aarch64": "https://example.com/macos-aarch64",
            },
        }
        expected = {
            "version": "0.1.1",
            "protocol": read_protocol_version(),
            "notes": "### Fixed\n- One",
            "assets": {
                "linux-x86_64": "https://example.com/linux-x86_64",
                "linux-aarch64": "https://example.com/linux-aarch64",
                "macos-x86_64": "https://example.com/macos-x86_64",
                "macos-aarch64": "https://example.com/macos-aarch64",
            },
        }

        canonical = ensure_manifest_matches_expected(actual, expected, "test manifest")
        self.assertEqual(canonical, expected)

    def test_current_release_assets_must_be_mirrored(self) -> None:
        assets = default_release_assets("0.1.1")
        ensure_current_release_assets_are_mirrored(
            {
                "version": "0.1.1",
                "protocol": read_protocol_version(),
                "notes": "### Fixed\n- One",
                "assets": assets,
                "releases": {
                    "0.1.1": {
                        "notes": "### Fixed\n- One",
                        "assets": assets,
                    }
                },
            },
            "test manifest",
        )

    def test_current_release_assets_must_match_top_level_assets(self) -> None:
        with self.assertRaisesRegex(ChangelogError, "assets must match top-level assets"):
            ensure_current_release_assets_are_mirrored(
                {
                    "version": "0.1.1",
                    "protocol": read_protocol_version(),
                    "notes": "### Fixed\n- One",
                    "assets": default_release_assets("0.1.1"),
                    "releases": {
                        "0.1.1": {
                            "notes": "### Fixed\n- One",
                            "assets": default_release_assets("0.1.0"),
                        }
                    },
                },
                "test manifest",
            )

    def test_ensure_manifest_matches_expected_rejects_different_notes(self) -> None:
        with self.assertRaisesRegex(ChangelogError, "does not match the published GitHub release manifest"):
            ensure_manifest_matches_expected(
                {
                    "version": "0.1.1",
                    "protocol": read_protocol_version(),
                    "notes": "### Fixed\n- Different",
                    "assets": {
                        "linux-x86_64": "https://example.com/linux-x86_64",
                        "linux-aarch64": "https://example.com/linux-aarch64",
                        "macos-x86_64": "https://example.com/macos-x86_64",
                        "macos-aarch64": "https://example.com/macos-aarch64",
                    },
                },
                {
                    "version": "0.1.1",
                    "protocol": read_protocol_version(),
                    "notes": "### Fixed\n- One",
                    "assets": {
                        "linux-x86_64": "https://example.com/linux-x86_64",
                        "linux-aarch64": "https://example.com/linux-aarch64",
                        "macos-x86_64": "https://example.com/macos-x86_64",
                        "macos-aarch64": "https://example.com/macos-aarch64",
                    },
                },
                "test manifest",
            )

    def test_canonicalize_manifest_requires_protocol(self) -> None:
        with self.assertRaisesRegex(ChangelogError, "missing an integer protocol"):
            canonicalize_manifest(
                {
                    "version": "0.1.1",
                    "notes": "### Fixed\n- One",
                    "assets": default_release_assets("0.1.1"),
                },
                "test manifest",
            )

    def test_ensure_manifest_matches_expected_rejects_different_protocol(self) -> None:
        actual = {
            "version": "0.1.1",
            "protocol": read_protocol_version() + 1,
            "notes": "### Fixed\n- One",
            "assets": default_release_assets("0.1.1"),
        }
        expected = {
            "version": "0.1.1",
            "protocol": read_protocol_version(),
            "notes": "### Fixed\n- One",
            "assets": default_release_assets("0.1.1"),
        }

        with self.assertRaisesRegex(ChangelogError, "does not match"):
            ensure_manifest_matches_expected(actual, expected, "test manifest")


class ManifestOwnershipTests(unittest.TestCase):
    """#506: a manifest may only advertise binaries this repository published."""

    def foreign_manifest(self) -> dict[str, object]:
        return {
            "version": "0.6.8",
            "protocol": 12,
            "notes": "### Fixed\n- Someone else's release",
            "assets": default_release_assets("0.6.8", FOREIGN_REPO),
            "releases": {
                "0.6.8": {
                    "notes": "### Fixed\n- Someone else's release",
                    "protocol": 12,
                    "assets": default_release_assets("0.6.8", FOREIGN_REPO),
                }
            },
        }

    def test_foreign_root_assets_are_refused_by_name(self) -> None:
        with self.assertRaisesRegex(ChangelogError, "not this repository's release asset"):
            ensure_manifest_assets_belong_to_repo(
                self.foreign_manifest(), DEFAULT_RELEASE_REPO, "website/latest.json"
            )

    def test_foreign_archived_releases_are_refused_by_version(self) -> None:
        manifest = self.foreign_manifest()
        manifest["assets"] = default_release_assets("0.6.8")

        with self.assertRaisesRegex(ChangelogError, "published by another repository: 0.6.8"):
            ensure_manifest_assets_belong_to_repo(manifest, DEFAULT_RELEASE_REPO, "test manifest")

    def test_foreign_release_entries_lists_only_the_foreign_ones(self) -> None:
        releases = {
            "0.6.8": {"notes": "theirs", "assets": default_release_assets("0.6.8", FOREIGN_REPO)},
            "0.6.7": {"notes": "theirs", "assets": default_release_assets("0.6.7", FOREIGN_REPO)},
            "1.0.0": {"notes": "ours", "assets": default_release_assets("1.0.0")},
            "0.9.0": {"notes": "ours, never had assets of its own"},
        }

        self.assertEqual(foreign_release_entries(releases, DEFAULT_RELEASE_REPO), ["0.6.7", "0.6.8"])

    def test_foreign_release_entries_ignores_a_non_object_archive(self) -> None:
        self.assertEqual(foreign_release_entries(None, DEFAULT_RELEASE_REPO), [])
        self.assertEqual(foreign_release_entries("nope", DEFAULT_RELEASE_REPO), [])

    def test_a_release_labelled_with_a_foreign_tag_is_refused(self) -> None:
        """Owned host, wrong tag: the entry claims v1.0.0 and serves v0.6.8's binary."""
        manifest = json.loads(build_no_release_manifest())
        manifest["assets"] = default_release_assets("0.6.8")

        with self.assertRaises(ChangelogError):
            ensure_manifest_assets_belong_to_repo(manifest, DEFAULT_RELEASE_REPO, "test manifest")


class CheckedInManifestTests(unittest.TestCase):
    def test_checked_in_latest_json_advertises_only_this_repository(self) -> None:
        manifest = load_json(DEFAULT_LATEST_JSON_PATH)
        ensure_manifest_assets_belong_to_repo(manifest, DEFAULT_RELEASE_REPO, str(DEFAULT_LATEST_JSON_PATH))

    def test_checked_in_latest_json_advertises_no_installable_version_yet(self) -> None:
        # Until the first release is cut, the stable manifest is a sentinel: a
        # `flk update` must find nothing to install, and remote bootstrap must say
        # the manifest has no entry for the running version rather than fetch a
        # binary. Cutting the first release replaces this file, and this test with
        # it — that is the transition being made explicit, not a failure to fix.
        manifest = load_json(DEFAULT_LATEST_JSON_PATH)
        self.assertEqual(manifest["version"], NO_STABLE_RELEASE_VERSION)
        self.assertEqual(manifest["protocol"], read_protocol_version())

    def test_no_release_manifest_is_script_output(self) -> None:
        self.assertEqual(
            load_json(DEFAULT_LATEST_JSON_PATH),
            json.loads(build_no_release_manifest()),
        )


class ReleasePlanTests(unittest.TestCase):
    """#509: what landed since the last release, and the bump it calls for."""

    def plan_for(self, subjects: list[tuple[str, str]]) -> object:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            for subject, body in subjects:
                commit(repo, subject, body)
            return build_release_plan(cwd=repo)

    def test_fix_only_recommends_a_patch(self) -> None:
        plan = self.plan_for([("fix(update): install the selected channel", "refs #506")])

        self.assertEqual(plan.bump, "patch")
        self.assertEqual(plan.recommended_version, None)  # no tag yet
        self.assertEqual(plan.groups["fix"], ["fix(update): install the selected channel"])

    def test_a_feat_recommends_a_minor(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v1.2.3")
            commit(repo, "feat(update): offer a preview channel")

            plan = build_release_plan(cwd=repo)

        self.assertEqual(plan.last_version, "1.2.3")
        self.assertEqual(plan.bump, "minor")
        self.assertEqual(plan.recommended_version, "1.3.0")

    def test_a_bang_recommends_a_major(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v1.2.3")
            commit(repo, "feat(config)!: remove the legacy config layer")

            plan = build_release_plan(cwd=repo)

        self.assertEqual(plan.bump, "major")
        self.assertEqual(plan.recommended_version, "2.0.0")
        self.assertEqual(plan.breaking, ["feat(config)!: remove the legacy config layer"])

    def test_a_breaking_change_footer_recommends_a_major(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v1.2.3")
            commit(repo, "refactor: rewrite the pane reader", "BREAKING CHANGE: the old reader is gone")

            plan = build_release_plan(cwd=repo)

        self.assertEqual(plan.bump, "major")
        self.assertEqual(plan.recommended_version, "2.0.0")

    def test_a_dash_spelled_breaking_footer_recommends_a_major(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v1.2.3")
            commit(repo, "fix: correct the pane reader", "BREAKING-CHANGE: the old reader is gone")

            plan = build_release_plan(cwd=repo)

        self.assertEqual(plan.bump, "major")

    def test_documentation_only_still_recommends_a_patch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v1.2.3")
            commit(repo, "docs(adr): record the release decision")
            commit(repo, "chore(deps): bump toml")

            plan = build_release_plan(cwd=repo)

        self.assertEqual(plan.bump, "patch")
        self.assertEqual(plan.recommended_version, "1.2.4")

    def test_the_strongest_signal_in_the_range_wins(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v1.2.3")
            commit(repo, "fix: a fix")
            commit(repo, "feat: a feature")
            commit(repo, "fix!: a break")

            plan = build_release_plan(cwd=repo)

        self.assertEqual(plan.bump, "major")
        self.assertEqual(plan.recommended_version, "2.0.0")

    def test_the_first_release_has_a_defined_answer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            commit(repo, "feat: the first feature")

            plan = build_release_plan(cwd=repo)

        self.assertTrue(plan.is_first_release)
        self.assertIsNone(plan.last_version)
        self.assertEqual(plan.bump, "minor")
        self.assertEqual(plan.recommended_version, None)  # no line to bump from
        self.assertEqual(plan.rev_range, "HEAD")

    def test_the_first_release_reads_the_whole_history(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            first = commit(repo, "feat: one")
            commit(repo, "feat: two")

            plan = build_release_plan(cwd=repo)

        self.assertEqual(plan.commit_count, 3)  # the fixture's own first commit included
        self.assertEqual(plan.groups["feat"], ["feat: one", "feat: two"])

    def test_an_empty_range_recommends_nothing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v1.0.0")

            plan = build_release_plan(cwd=repo)

        self.assertEqual(plan.commit_count, 0)
        self.assertIsNone(plan.bump)
        self.assertIsNone(plan.recommended_version)

    def test_release_commits_do_not_count_towards_their_own_bump(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v1.0.0")
            commit(repo, "release: v1.0.1")

            plan = build_release_plan(cwd=repo)

        self.assertIsNone(plan.bump)

    def test_an_unconventional_subject_is_reported_and_contributes_nothing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v1.0.0")
            commit(repo, "update preview channel")

            plan = build_release_plan(cwd=repo)

        self.assertEqual(plan.unclassified, ["update preview channel"])
        self.assertIsNone(plan.bump)

    def test_an_explicit_ref_overrides_the_tag(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v1.0.0")
            marker = commit(repo, "chore: marker")
            commit(repo, "feat: after the marker")

            plan = build_release_plan(since=marker, cwd=repo)

        self.assertIsNone(plan.last_version)
        self.assertEqual(plan.bump, "minor")
        self.assertEqual(plan.groups["feat"], ["feat: after the marker"])

    def test_resolve_release_base_reads_the_tag_as_the_range(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            tag(repo, "v2.0.0")

            rev_range, label, last_version, is_first = resolve_release_base(cwd=repo)

        self.assertEqual(rev_range, "v2.0.0..HEAD")
        self.assertEqual(last_version, "2.0.0")
        self.assertFalse(is_first)
        self.assertIn("v2.0.0", label)


class VersionArithmeticTests(unittest.TestCase):
    def test_bump_version(self) -> None:
        self.assertEqual(bump_version("1.2.3", "major"), "2.0.0")
        self.assertEqual(bump_version("1.2.3", "minor"), "1.3.0")
        self.assertEqual(bump_version("1.2.3", "patch"), "1.2.4")

    def test_bump_version_rejects_an_unknown_bump(self) -> None:
        with self.assertRaisesRegex(ChangelogError, "unknown bump"):
            bump_version("1.2.3", "epoch")


class VersionCheckTests(unittest.TestCase):
    def plan_with(self, last_version: str | None, bump: str | None) -> object:
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo(Path(tmp))
            if last_version is not None:
                tag(repo, f"v{last_version}")
                if bump == "minor":
                    commit(repo, "feat: a feature")
                elif bump == "patch":
                    commit(repo, "fix: a fix")
                elif bump == "major":
                    commit(repo, "feat!: a break")
            return build_release_plan(cwd=repo)

    def test_a_version_at_or_below_the_last_release_is_refused(self) -> None:
        plan = self.plan_with("1.2.3", "patch")

        with self.assertRaisesRegex(ChangelogError, "not greater than the last release"):
            ensure_version_is_releasable(plan, "1.2.3", set())
        with self.assertRaises(ChangelogError):
            ensure_version_is_releasable(plan, "1.0.0", set())

    def test_a_version_below_the_recommendation_is_refused(self) -> None:
        plan = self.plan_with("1.2.3", "minor")

        with self.assertRaisesRegex(ChangelogError, "below the recommended v1.3.0"):
            ensure_version_is_releasable(plan, "1.2.4", set())

    def test_the_recommended_version_is_accepted(self) -> None:
        plan = self.plan_with("1.2.3", "minor")

        ensure_version_is_releasable(plan, "1.3.0", set())
        ensure_version_is_releasable(plan, "2.0.0", set())

    def test_holding_a_release_back_is_overridable(self) -> None:
        plan = self.plan_with("1.2.3", "minor")

        ensure_version_is_releasable(plan, "1.2.4", {"allow-below-recommended"})

    def test_a_held_back_release_still_has_to_be_greater(self) -> None:
        plan = self.plan_with("1.2.3", "minor")

        with self.assertRaises(ChangelogError):
            ensure_version_is_releasable(plan, "1.2.3", {"allow-below-recommended"})

    def test_the_first_release_is_never_refused_for_want_of_an_earlier_version(self) -> None:
        plan = self.plan_with(None, None)

        ensure_version_is_releasable(plan, "0.0.1", set())
        ensure_version_is_releasable(plan, "1.0.0", set())


if __name__ == "__main__":
    unittest.main()
