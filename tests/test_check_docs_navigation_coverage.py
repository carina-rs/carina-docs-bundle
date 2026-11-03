import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "check-docs-navigation-coverage.py"
SPEC = importlib.util.spec_from_file_location("check_docs_navigation_coverage", SCRIPT)
CHECKER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECKER)


class DocsNavigationCoverageTests(unittest.TestCase):
    def setUp(self):
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary_directory.name)

    def tearDown(self):
        self.temporary_directory.cleanup()

    def run_check(
        self,
        *,
        bundle_resources=(),
        bundle_data_sources=(),
        metadata_resources=(),
        metadata_data_sources=(),
    ):
        metadata_path = self.root / "docs-navigation" / "carina-rs" / "aws.json"
        metadata_path.parent.mkdir(parents=True)
        metadata_path.write_text(
            json.dumps(
                {
                    "services": [
                        {
                            "id": "test",
                            "title": "Test",
                            "resources": list(metadata_resources),
                            "data_sources": list(metadata_data_sources),
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )

        bundle_path = self.root / "docs.tar.gz"
        with tarfile.open(bundle_path, mode="w:gz") as archive:
            for member_name in (
                "index.md",
                *(f"resources/{slug}.md" for slug in bundle_resources),
                *(f"data-sources/{slug}.md" for slug in bundle_data_sources),
            ):
                info = tarfile.TarInfo(member_name)
                info.size = 0
                archive.addfile(info, io.BytesIO())

        stdout = io.StringIO()
        stderr = io.StringIO()
        exit_code = CHECKER.check_coverage(
            "carina-rs",
            "aws",
            bundle_path,
            root=self.root,
            stdout=stdout,
            stderr=stderr,
        )
        return exit_code, stdout.getvalue(), stderr.getvalue()

    def test_all_bundle_pages_are_covered(self):
        exit_code, stdout, stderr = self.run_check(
            bundle_resources=("ec2_vpc",),
            bundle_data_sources=("iam_roles",),
            metadata_resources=("ec2_vpc",),
            metadata_data_sources=("iam_roles",),
        )

        self.assertEqual(exit_code, 0)
        self.assertIn("covers every page", stdout)
        self.assertEqual(stderr, "")

    def test_missing_resource_fails_and_names_slug(self):
        exit_code, _, stderr = self.run_check(
            bundle_resources=("ec2_foo",),
            bundle_data_sources=("iam_roles",),
            metadata_data_sources=("iam_roles",),
        )

        self.assertEqual(exit_code, 1)
        self.assertIn("resources missing", stderr)
        self.assertIn("ec2_foo", stderr)

    def test_missing_data_source_fails(self):
        exit_code, _, stderr = self.run_check(
            bundle_resources=("ec2_vpc",),
            bundle_data_sources=("ec2_instances",),
            metadata_resources=("ec2_vpc",),
        )

        self.assertEqual(exit_code, 1)
        self.assertIn("data sources missing", stderr)
        self.assertIn("ec2_instances", stderr)

    def test_extra_metadata_slug_is_informational(self):
        exit_code, stdout, stderr = self.run_check(
            bundle_resources=("ec2_vpc",),
            metadata_resources=("ec2_vpc", "ec2_old_version_only"),
        )

        self.assertEqual(exit_code, 0)
        self.assertIn("note:", stdout)
        self.assertIn("ec2_old_version_only", stdout)
        self.assertEqual(stderr, "")


if __name__ == "__main__":
    unittest.main()
