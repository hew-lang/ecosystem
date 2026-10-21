#!/usr/bin/env python3
"""Build only missing signed native attachments with an official Hew SDK."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
HOSTS = {
    "x86_64": {
        "runner": "ubuntu-24.04",
        "target": "x86_64-unknown-linux-gnu",
        "archive": "linux-x86_64",
    },
    "aarch64": {
        "runner": "ubuntu-24.04-arm",
        "target": "aarch64-unknown-linux-gnu",
        "archive": "linux-aarch64",
    },
}
SYSTEM = {
    "image/magick": "ImageMagick 7 (MagickWand), its delegates and development headers",
    "storage/s3": "OpenSSL development headers and runtime libraries",
}


def packages(root=None):
    root = root or ROOT
    result = {}
    for path in sorted([*root.glob("*/hew.toml"), *root.glob("*/*/hew.toml")]):
        relative = path.parent.relative_to(root).as_posix()
        if relative.startswith("hew/"):
            continue
        manifest = tomllib.loads(path.read_text())
        if manifest.get("native", {}).get("crate"):
            result[relative] = manifest["package"]
    return result


def selected(value, available):
    names = list(available) if value == "all" else value.replace(",", " ").split()
    if not names or len(set(names)) != len(names) or set(names) - set(available):
        raise ValueError(
            "Select all or unique native package paths: " + ", ".join(available)
        )
    return names


def run(args, **kwargs):
    subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def batch(args):
    sdk = args.sdk.resolve()
    hew = sdk / "bin/hew"
    version = subprocess.check_output([str(hew), "--version"], text=True).strip()
    if version != f"hew {args.version}":
        raise ValueError(
            f"Official SDK version mismatch: expected hew {args.version}, got {version}"
        )
    machine = {"arm64": "aarch64", "amd64": "x86_64"}.get(
        platform.machine(), platform.machine()
    )
    if platform.system() != "Linux" or machine != args.host:
        raise ValueError("Native releases must build on their selected Linux host")
    available = packages()
    cohort = selected(args.packages, available)
    args.output.mkdir(parents=True, exist_ok=True)
    # The publisher owns source identity; install owns signature, descriptor and
    # native-cache verification. The adapter consumes their checked lock facts.
    for path in cohort:
        run([hew, "publish", "--dry-run"], cwd=ROOT / path)
    results = []

    def record(result):
        results.append(result)
        (args.output / "receipt.json").write_text(
            json.dumps(
                {
                    "sdk_version": args.version,
                    "sdk_archive_sha256": os.environ.get("NATIVE_SDK_CHECKSUM"),
                    "host": args.host,
                    "packages": results,
                },
                indent=2,
            )
            + "\n"
        )

    with tempfile.TemporaryDirectory(prefix="native-release-") as temporary:
        project = Path(temporary)
        manifest = '[package]\nname = "native_release_probe"\nversion = "0.1.0"\nedition = "2026"\n\n[dependencies]\n'
        manifest += "".join(
            f'{json.dumps(available[path]["name"])} = "={available[path]["version"]}"\n'
            for path in cohort
        )
        (project / "hew.toml").write_text(manifest)
        run(
            [hew, "install"],
            cwd=project,
            env=os.environ | {"HEW_HOME": str(project / "profile")},
        )
        locked = {
            item["name"]: item
            for item in tomllib.loads((project / "hew.lock").read_text())["package"]
        }
        dependencies = set()
        for path in cohort:
            package = available[path]
            native = locked[package["name"]].get("native", [])
            if native:
                if (
                    len(native) != 1
                    or native[0]["identity"]["hew"] != args.version
                    or native[0]["identity"]["target"] != HOSTS[args.host]["target"]
                ):
                    raise ValueError(f"Unexpected verified native identity for {path}")
                record(
                    {"package": path, "state": "verified-existing", "native": native[0]}
                )
                continue
            if args.install_system_deps:
                kind = "magick" if path == "image/magick" else "base"
                if kind not in dependencies:
                    run(["bash", ROOT / "scripts/setup-native-system-deps.sh", kind])
                    dependencies.add(kind)
                    dependencies.add("base")
            elif path in SYSTEM:
                probe = "MagickWand" if path == "image/magick" else "openssl"
                run(
                    [
                        "pkg-config",
                        "--atleast-version=" + ("7" if probe == "MagickWand" else "1"),
                        probe,
                    ]
                )
            archive = (
                args.output.resolve()
                / f"{package['name']}-{args.version}-{args.host}.tar.zst"
            )
            run(
                [
                    hew,
                    "native",
                    "build",
                    "--target",
                    HOSTS[args.host]["target"],
                    "--output",
                    archive,
                ],
                cwd=ROOT / path,
            )
            state = "built"
            if args.publish:
                run(
                    [
                        hew,
                        "publish",
                        "--auth",
                        "github-actions",
                        "--native-archive",
                        archive,
                    ],
                    cwd=ROOT / path,
                )
                state = "published"
            record(
                {
                    "package": path,
                    "state": state,
                    "archive": str(archive),
                    "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
                    "system_dependencies": SYSTEM.get(path),
                }
            )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    plan = sub.add_parser("plan")
    plan.add_argument("--packages", default="all")
    plan.add_argument("--hosts", choices=[*HOSTS, "both"], default="both")
    plan.add_argument("--github-output", type=Path)
    build = sub.add_parser("batch")
    build.add_argument("--packages", required=True)
    build.add_argument("--host", choices=HOSTS, required=True)
    build.add_argument("--sdk", type=Path, required=True)
    build.add_argument("--version", required=True)
    build.add_argument("--output", type=Path, required=True)
    build.add_argument("--publish", action="store_true")
    build.add_argument("--install-system-deps", action="store_true")
    args = parser.parse_args()
    if args.command == "batch":
        batch(args)
        return
    available = packages()
    cohort = selected(args.packages, available)
    matrix = {
        "include": [
            dict(host=host, **HOSTS[host])
            for host in (HOSTS if args.hosts == "both" else [args.hosts])
        ]
    }
    print(
        json.dumps(
            {
                "matrix": matrix,
                "packages": [
                    {
                        "path": path,
                        "name": available[path]["name"],
                        "system_dependencies": SYSTEM.get(path),
                    }
                    for path in cohort
                ],
            },
            indent=2,
        )
    )
    if args.github_output:
        with args.github_output.open("a") as output:
            output.write("matrix=" + json.dumps(matrix) + "\n")


if __name__ == "__main__":
    main()
