#!/usr/bin/env python3
"""Check that a provider docs bundle is covered by navigation metadata."""

import argparse
import json
import sys
import tarfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SECTIONS = (
    ("resources", "resources", "resources"),
    ("data-sources", "data_sources", "data sources"),
)


class CoverageError(Exception):
    """A clear error that prevents the coverage check from running."""


def _bundle_slugs(bundle_path):
    slugs = {metadata_key: set() for _, metadata_key, _ in SECTIONS}
    try:
        with tarfile.open(bundle_path, mode="r:gz") as archive:
            members = archive.getmembers()
    except (OSError, tarfile.TarError) as error:
        raise CoverageError(f"cannot read docs bundle {bundle_path}: {error}") from error

    for member in members:
        if not member.isfile():
            continue
        for directory, metadata_key, _ in SECTIONS:
            prefix = f"{directory}/"
            if not member.name.startswith(prefix) or not member.name.endswith(".md"):
                continue
            filename = member.name[len(prefix) :]
            if "/" not in filename and filename != ".md":
                slugs[metadata_key].add(filename[: -len(".md")])
    return slugs


def _metadata_slugs(metadata_path):
    try:
        document = json.loads(metadata_path.read_text(encoding="utf-8"))
    except OSError as error:
        raise CoverageError(f"cannot read {metadata_path}: {error}") from error
    except json.JSONDecodeError as error:
        raise CoverageError(f"invalid JSON in {metadata_path}: {error}") from error

    if not isinstance(document, dict) or not isinstance(document.get("services"), list):
        raise CoverageError(f"{metadata_path}: expected an object with a services array")

    slugs = {metadata_key: set() for _, metadata_key, _ in SECTIONS}
    for index, service in enumerate(document["services"]):
        if not isinstance(service, dict):
            raise CoverageError(f"{metadata_path}: services[{index}] must be an object")
        for _, metadata_key, _ in SECTIONS:
            values = service.get(metadata_key)
            if not isinstance(values, list) or any(
                not isinstance(value, str) for value in values
            ):
                raise CoverageError(
                    f"{metadata_path}: services[{index}].{metadata_key} "
                    "must be an array of strings"
                )
            slugs[metadata_key].update(values)
    return slugs


def check_coverage(
    namespace,
    name,
    bundle_path,
    *,
    root=ROOT,
    stdout=None,
    stderr=None,
):
    """Return zero when every bundle page is assigned in the metadata."""

    stdout = sys.stdout if stdout is None else stdout
    stderr = sys.stderr if stderr is None else stderr
    provider = f"{namespace}/{name}"
    relative_metadata_path = Path("docs-navigation") / namespace / f"{name}.json"
    metadata_path = Path(root) / relative_metadata_path

    if not metadata_path.is_file():
        print(
            f"{provider}: metadata file is missing: {relative_metadata_path}",
            file=stderr,
        )
        return 1

    try:
        bundle_slugs = _bundle_slugs(Path(bundle_path))
        metadata_slugs = _metadata_slugs(metadata_path)
    except CoverageError as error:
        print(f"{provider}: {error}", file=stderr)
        return 1

    has_missing = False
    for _, metadata_key, label in SECTIONS:
        missing = sorted(bundle_slugs[metadata_key] - metadata_slugs[metadata_key])
        extra = sorted(metadata_slugs[metadata_key] - bundle_slugs[metadata_key])
        if missing:
            has_missing = True
            print(
                f"{provider}: {label} missing from {relative_metadata_path}: "
                f"{', '.join(missing)}",
                file=stderr,
            )
        if extra:
            print(
                f"{provider}: note: {label} present in {relative_metadata_path} "
                "but not in this bundle (allowed for metadata shared across versions): "
                f"{', '.join(extra)}",
                file=stdout,
            )

    if has_missing:
        print(
            f"{provider}: add each missing slug to a service in "
            f"{relative_metadata_path}",
            file=stderr,
        )
        return 1

    print(f"{provider}: docs navigation covers every page in the bundle", file=stdout)
    return 0


def _parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("namespace")
    parser.add_argument("name")
    parser.add_argument("bundle", type=Path)
    return parser.parse_args(argv)


def main(argv=None):
    args = _parse_args(argv)
    return check_coverage(args.namespace, args.name, args.bundle)


if __name__ == "__main__":
    raise SystemExit(main())
