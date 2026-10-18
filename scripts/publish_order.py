#!/usr/bin/env python3
"""Print every tracked package manifest as "manifest<TAB>name<TAB>version", dependencies first.

A stopgap for scripts/publish-packages.sh until `hew` publishes a workspace itself.
"""

from __future__ import annotations

import pathlib
import subprocess
import tomllib
from graphlib import CycleError, TopologicalSorter


def main() -> None:
    output = subprocess.check_output(["git", "ls-files", "--", "*hew.toml"], text=True)
    packages = {}
    for path in map(pathlib.Path, output.splitlines()):
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


if __name__ == "__main__":
    main()
