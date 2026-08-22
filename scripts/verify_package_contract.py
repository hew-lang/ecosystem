#!/usr/bin/env python3
"""Verify the dotted package metadata and canonical registry URL contract."""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys
import tomllib


# The two counts this repository asserts about its own tree. They live here,
# together, because a count kept in a second file drifts from this one:
# `--source-count` hands the source count to scripts/corpus-gate.sh so the gate
# does not restate it. Adding a package moves one or both, deliberately.
EXPECTED_MANIFESTS = 15
EXPECTED_SOURCES = 52
EXPECTED_VERSION = "0.3.0"
VEC_NEW = re.compile(r"\bVec::new\b")
QUOTED_STRING = re.compile(r'"(?:[^"\\]|\\.)*"')
SQL_KEYWORD = re.compile(
    r"\b(?:alter|create|delete|from|insert|select|update|values|where|with)\b",
    re.IGNORECASE,
)
# Keep this deliberately narrower than PostgreSQL's full cast grammar. Expanding
# the repository's accepted casts must be an explicit contract change rather
# than an exemption that can also hide a legacy package path.
SQL_CAST = re.compile(r"(?:\$\d+|\d+|value)::(?:bigint|text)\b")
LEGACY_METADATA_PATH = re.compile(r"\b(?:hew|ecosystem)::")


# The dotted-name grammar. Every published name is at least two components
# (a namespace and a package), each starting with a lowercase letter and
# continuing in lowercase alphanumerics or underscores. `/` is the registry's
# canonical separator and must never appear in a source name; `::` is the
# retired spelling. This function is the single authority for the grammar --
# `--check-name` exposes it to shell callers so the publish loop asks the same
# question rather than re-implementing a weaker one.
NAME_COMPONENT = re.compile(r"^[a-z][a-z0-9_]*$")


def registry_path(name: str) -> str:
    if "::" in name:
        raise ValueError(f"legacy package name is forbidden: {name}")
    if "/" in name:
        raise ValueError(f"package name must not contain '/': {name}")
    parts = name.split(".")
    if len(parts) < 2:
        raise ValueError(
            f"package name must have at least two dotted components: {name}"
        )
    for part in parts:
        if not NAME_COMPONENT.match(part):
            raise ValueError(
                f"invalid component {part!r} in package name {name}: components "
                "must match [a-z][a-z0-9_]*"
            )
    return "/".join(parts)


def verify_name_grammar() -> None:
    accepted = ["hew.dag", "hew.db.postgres", "hew.alpha.beta.gamma", "hew.math.stats"]
    for name in accepted:
        registry_path(name)

    rejected = [
        "hew",                  # single component
        "hew::db::postgres",    # retired spelling
        "hew/db/postgres",      # canonical separator, not a source name
        "hew..dag",             # empty component
        ".dag",                 # empty leading component
        "hew.Dag",              # uppercase
        "hew.2fast",            # leading digit
        "hew.d-b",              # hyphen
        "hew.db ",              # trailing space
    ]
    for name in rejected:
        try:
            registry_path(name)
        except ValueError:
            continue
        raise SystemExit(f"package name grammar accepted an invalid name: {name}")


def tracked_manifests() -> list[pathlib.Path]:
    output = subprocess.check_output(
        ["git", "ls-files", "--", "*hew.toml"], text=True
    )
    return [pathlib.Path(line) for line in output.splitlines()]


def tracked_hew_sources() -> list[pathlib.Path]:
    output = subprocess.check_output(["git", "ls-files", "--", "*.hew"], text=True)
    return [pathlib.Path(line) for line in output.splitlines()]


def sql_cast_positions(line: str) -> set[int]:
    positions: set[int] = set()
    for quoted in QUOTED_STRING.finditer(line):
        text = quoted.group()
        if not SQL_KEYWORD.search(text):
            continue
        for cast in SQL_CAST.finditer(text):
            positions.add(quoted.start() + cast.start() + cast.group().index("::"))
    return positions


def legacy_double_colons(line: str) -> list[int]:
    allowed = sql_cast_positions(line)
    allowed.update(match.start() + match.group().index("::") for match in VEC_NEW.finditer(line))
    return [match.start() for match in re.finditer(r"::", line) if match.start() not in allowed]


def verify_legacy_syntax_detector() -> None:
    cases = {
        "CellValue::Null": True,
        "redis.PipelineCommand::Set(\"key\", value)": True,
        "import hew::db::redis;": True,
        "let values: Vec<i64> = Vec::new();": False,
        'db.query("SELECT 41::bigint AS value")': False,
        'db.query("SELECT value::text FROM records")': False,
        'db.query("SELECT $1::text")': False,
        'let text = "SELECT 1; hew::db";': True,
        'let text = "SELECT hew::text";': True,
        'let text = "SELECT value::custom_type";': True,
        "let values = Vec::new(); let lookup = Lookup::Missing;": True,
    }
    for line, should_reject in cases.items():
        rejected = bool(legacy_double_colons(line))
        if rejected != should_reject:
            raise SystemExit(f"legacy syntax detector synthetic check failed: {line}")

    metadata_cases = {
        "Hew ecosystem::db::mysql client": True,
        "hew::db::mysql client": True,
        "Crates.io category multimedia::images": False,
    }
    for description, should_reject in metadata_cases.items():
        rejected = bool(LEGACY_METADATA_PATH.search(description))
        if rejected != should_reject:
            raise SystemExit(
                f"package metadata detector synthetic check failed: {description}"
            )


def workspace_version() -> str:
    manifest = tomllib.loads(pathlib.Path("Cargo.toml").read_text())
    return manifest["workspace"]["package"]["version"]


def verify_native_crates(package_roots: set[pathlib.Path]) -> None:
    """Every native-backed package agrees with its Cargo crate.

    `hew.toml`'s `[native]` names the crate directory and the library symbol
    the compiler links; `Cargo.toml` decides what that library is actually
    called. Nothing else compares the two, so they can drift apart and only
    fail at link time in a consumer's build.
    """
    expected_workspace_version = workspace_version()
    violations: list[str] = []
    for root in sorted(package_roots):
        manifest = tomllib.loads((root / "hew.toml").read_text())
        cargo_path = root / "Cargo.toml"
        if not cargo_path.is_file():
            if "native" in manifest:
                violations.append(f"{root}: declares [native] but has no Cargo.toml")
            continue

        cargo = tomllib.loads(cargo_path.read_text())
        version = cargo["package"]["version"]
        if version == {"workspace": True}:
            version = expected_workspace_version
        if version != EXPECTED_VERSION:
            violations.append(
                f"{cargo_path}: expected version {EXPECTED_VERSION}, found {version}"
            )

        native = manifest.get("native")
        if native is None:
            continue
        crate_root = (root / native["crate"]).resolve()
        if crate_root != cargo_path.parent.resolve():
            violations.append(
                f"{root}/hew.toml: [native] crate must be \".\", found {native['crate']!r}"
            )
        library = cargo.get("lib", {}).get("name")
        if library != native["lib"]:
            violations.append(
                f"{root}: hew.toml links {native['lib']!r} but Cargo.toml builds {library!r}"
            )
        if native["kind"] not in cargo.get("lib", {}).get("crate-type", []):
            violations.append(
                f"{root}: hew.toml wants a {native['kind']} that Cargo.toml does not build"
            )

    if violations:
        raise SystemExit("package/crate contract broken:\n" + "\n".join(violations))


def verify_active_package_text(package_roots: set[pathlib.Path]) -> None:
    tracked_sources = set(tracked_hew_sources())
    violations: list[str] = []
    for root in sorted(package_roots):
        readme = root / "README.md"
        if readme.is_file():
            for number, line in enumerate(readme.read_text().splitlines(), start=1):
                if legacy_double_colons(line):
                    violations.append(f"{readme}:{number}: legacy :: spelling")

        cargo_manifest = root / "Cargo.toml"
        if cargo_manifest.is_file():
            description = tomllib.loads(cargo_manifest.read_text()).get("package", {}).get(
                "description", ""
            )
            if LEGACY_METADATA_PATH.search(description):
                violations.append(f"{cargo_manifest}: package description uses legacy :: spelling")

        for source in sorted(path for path in tracked_sources if root in path.parents):
            for number, line in enumerate(source.read_text().splitlines(), start=1):
                if legacy_double_colons(line):
                    violations.append(f"{source}:{number}: legacy :: spelling")

    if violations:
        raise SystemExit("active package text uses legacy :: spelling:\n" + "\n".join(violations))


def check_name(name: str) -> int:
    """Grammar check for one name, for shell callers. Silent on success."""
    try:
        registry_path(name)
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[0] == "--check-name":
        return check_name(argv[1])
    if argv == ["--source-count"]:
        print(EXPECTED_SOURCES)
        return 0
    if argv:
        raise SystemExit(
            f"usage: {sys.argv[0]} [--check-name NAME | --source-count]"
        )

    verify_legacy_syntax_detector()
    verify_name_grammar()
    manifests = tracked_manifests()
    if len(manifests) != EXPECTED_MANIFESTS:
        raise SystemExit(
            f"expected {EXPECTED_MANIFESTS} tracked package manifests, found {len(manifests)}"
        )

    for manifest in manifests:
        package = tomllib.loads(manifest.read_text())["package"]
        name = package["name"]
        version = package["version"]
        if "::" in name:
            raise SystemExit(f"{manifest}: legacy package name is forbidden: {name}")
        if version != EXPECTED_VERSION:
            raise SystemExit(
                f"{manifest}: expected version {EXPECTED_VERSION}, found {version}"
            )
        try:
            registry_path(name)
        except ValueError as error:
            raise SystemExit(f"{manifest}: {error}") from error

    if registry_path("hew.alpha.beta.gamma") != "hew/alpha/beta/gamma":
        raise SystemExit("dotted registry mapping is not depth-independent")

    sources = tracked_hew_sources()
    if len(sources) != EXPECTED_SOURCES:
        raise SystemExit(
            f"expected {EXPECTED_SOURCES} tracked Hew sources, found {len(sources)}"
        )

    roots = {manifest.parent for manifest in manifests}
    verify_native_crates(roots)
    verify_active_package_text(roots)

    print(
        f"verified {len(manifests)} dotted manifests over {len(sources)} Hew "
        f"sources, their Cargo crates, and active package text at version "
        f"{EXPECTED_VERSION}; registry URLs are slash-canonical"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
