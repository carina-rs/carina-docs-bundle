#!/usr/bin/env python3
"""Check the standalone package's reachable dependencies, including dev/build edges."""

import re
import subprocess


def plain(output):
    return re.sub(r"\x1b\[[0-9;]*m", "", output)


def check_tree(output):
    forbidden = re.compile(
        r"^(?:aws-|"
        r"carina-registry$|axum(?:-|$)|tower(?:-|$)|hyper(?:-|$)|"
        r"reqwest$|lambda(?:_|-|$))"
    )
    packages = {line.split()[0] for line in plain(output).splitlines() if line.strip()}
    if "carina-docs-bundle" not in packages:
        raise ValueError("the forward tree did not contain the checker package")
    rejected = sorted(package for package in packages if forbidden.match(package))
    if rejected:
        raise ValueError("server dependencies reachable from checker: " + ", ".join(rejected))


def check_features(output):
    output = plain(output)
    if 'tokio feature "' not in output:
        raise ValueError("the feature tree did not show Tokio features")
    if re.search(r'tokio feature "full"', output):
        raise ValueError("the isolated checker activates tokio/full")


def check_negative_tree(result):
    expected = "package ID specification `rustls-webpki@0.101.7` did not match any packages"
    stderr = plain(result.stderr)
    if result.returncode == 0 or expected not in stderr:
        raise ValueError("legacy rustls-webpki absence was not proven: " + result.stderr)


def main():
    for features in ([], ["--all-features"]):
        base = ["cargo", "tree", "--locked", *features,
                "--target", "all", "--prefix", "none", "--format", "{p}"]
        result = subprocess.run([*base, "--edges", "normal,build,dev"],
                                check=True, capture_output=True, text=True)
        check_tree(result.stdout)
        result = subprocess.run([*base, "--edges", "normal,build,dev,features"],
                                check=True, capture_output=True, text=True)
        check_features(result.stdout)
    result = subprocess.run(["cargo", "tree", "--locked", "-i", "rustls-webpki@0.101.7"],
                            capture_output=True, text=True)
    check_negative_tree(result)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.stderr or str(error)) from error
    except ValueError as error:
        raise SystemExit(str(error)) from error
