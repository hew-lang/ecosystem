#!/usr/bin/env python3
"""Check package manifests against their Cargo crates, and order them for publish.

    verify_package_contract.py                  check every tracked manifest
    verify_package_contract.py --check-name N   validate one package name
    verify_package_contract.py --publish-order  print "manifest<TAB>name<TAB>version",
                                                dependencies first
"""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys
import tomllib
from graphlib import CycleError, TopologicalSorter

# Two or more dotted components: a namespace, then the package path.
NAME = re.compile(r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$")


def tracked_manifests() -> list[pathlib.Path]:
    output = subprocess.check_output(["git", "ls-files", "--", "*hew.toml"], text=True)
    return [pathlib.Path(line) for line in output.splitlines()]


def workspace_version() -> str:
    manifest = tomllib.loads(pathlib.Path("Cargo.toml").read_text())
    return manifest["workspace"]["package"]["version"]


def publish_order() -> None:
    packages = {}
    for path in tracked_manifests():
        manifest = tomllib.loads(path.read_text())
        name = manifest["package"]["name"]
        if name in packages:
            raise SystemExit(f"duplicate package {name}: {packages[name][0]} and {path}")
        packages[name] = (path, manifest)
    dependencies = {
        name: sorted(set(manifest.get("dependencies", {})) & packages.keys())
        for name, (_, manifest) in sorted(packages.items())
    }
    try:
        order = tuple(TopologicalSorter(dependencies).static_order())
    except CycleError as error:
        raise SystemExit(f"package dependency cycle: {error.args[1]}") from error
    for name in order:
        path, manifest = packages[name]
        print(f"{path}\t{name}\t{manifest['package']['version']}")


def verify_manifests() -> int:
    """Each package's name and version are valid, and its hew.toml agrees with its crate.

    `[native]` names the crate directory and the library the compiler links;
    Cargo.toml decides what that library is called. Nothing else compares them,
    so a mismatch would only surface as a link error in a consumer's build.
    """
    manifests = tracked_manifests()
    if not manifests:
        raise SystemExit("no tracked package manifests found")

    expected_version = workspace_version()
    violations: list[str] = []
    for path in manifests:
        root = path.parent
        manifest = tomllib.loads(path.read_text())
        package = manifest["package"]
        if not NAME.match(package["name"]):
            violations.append(f"{path}: invalid package name {package['name']!r}")
        if package["version"] != expected_version:
            violations.append(
                f"{path}: version {package['version']}, workspace is {expected_version}"
            )

        cargo_path = root / "Cargo.toml"
        native = manifest.get("native")
        if not cargo_path.is_file():
            if native:
                violations.append(f"{path}: declares [native] but {root} has no Cargo.toml")
            continue
        if not native:
            continue

        cargo = tomllib.loads(cargo_path.read_text())
        if (root / native["crate"]).resolve() != root.resolve():
            violations.append(f"{path}: [native] crate must be \".\", found {native['crate']!r}")
        lib = cargo.get("lib", {})
        if lib.get("name") != native["lib"]:
            violations.append(
                f"{root}: hew.toml links {native['lib']!r}, Cargo.toml builds {lib.get('name')!r}"
            )
        if native["kind"] not in lib.get("crate-type", []):
            violations.append(f"{root}: Cargo.toml does not build a {native['kind']}")

    if violations:
        raise SystemExit("package contract broken:\n" + "\n".join(violations))
    print(f"package contract ok: {len(manifests)} packages at {expected_version}")
    return 0


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[0] == "--check-name":
        if NAME.match(argv[1]):
            return 0
        print(f"invalid package name: {argv[1]}", file=sys.stderr)
        return 1
    if argv == ["--publish-order"]:
        publish_order()
        return 0
    if argv:
        raise SystemExit(f"usage: {sys.argv[0]} [--check-name NAME | --publish-order]")
    return verify_manifests()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
