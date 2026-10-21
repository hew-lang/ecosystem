"""Exercise cohort selection and existing CLI boundaries without registry writes."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location(
    "native_release", ROOT / "scripts/native-release.py"
)
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)

FAKE_HEW = """#!/usr/bin/env python3
import json,os,pathlib,sys,tomllib
args=sys.argv[1:]
with open(os.environ['FAKE_LOG'],'a') as log: log.write(json.dumps(args)+'\\n')
if args==['--version']:
 print('hew '+os.environ.get('FAKE_VERSION','0.6.4'));sys.exit(0)
if args==['publish','--dry-run'] and os.environ.get('FAKE_SOURCE_CHANGED'):
 sys.exit(1)
if args==['install']:
 if os.environ.get('FAKE_INSTALL_FAILED'):sys.exit(1)
 deps=tomllib.loads(pathlib.Path('hew.toml').read_text())['dependencies']
 text=''
 for name in deps:
  text+='[[package]]\\nname='+json.dumps(name)+'\\nversion="0.4.0"\\n'
  if os.environ.get('FAKE_EXISTING'):
   text+='[[package.native]]\\nsource_checksum="sha256:source"\\nchecksum="sha256:archive"\\n[package.native.identity]\\nhew="0.6.4"\\ntarget='+json.dumps(os.environ.get('FAKE_TARGET','x86_64-unknown-linux-gnu'))+'\\nrustc_release="1.96.0"\\nrustc_host="x86_64-unknown-linux-gnu"\\n'
 pathlib.Path('hew.lock').write_text(text)
if args[:2]==['native','build']:
 pathlib.Path(args[args.index('--output')+1]).write_bytes(b'native archive')
"""


class NativeRelease(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for path in ["net/http", "image/magick"]:
            directory = self.root / path
            directory.mkdir(parents=True)
            (directory / "hew.toml").write_text(
                '[package]\nname="hew.'
                + path.replace("/", ".")
                + '"\nversion="0.4.0"\n[native]\ncrate="."\n'
            )
        self.sdk = self.root / "sdk"
        (self.sdk / "bin").mkdir(parents=True)
        (self.sdk / "bin/hew").write_text(FAKE_HEW)
        (self.sdk / "bin/hew").chmod(0o755)
        self.log = self.root / "calls.jsonl"
        self.args = argparse.Namespace(
            sdk=self.sdk,
            version="0.6.4",
            host="x86_64",
            packages="net/http",
            output=self.root / "output",
            publish=True,
            install_system_deps=False,
        )
        self.environment = {"FAKE_LOG": str(self.log)}

    def execute(self):
        with (
            patch.object(release, "ROOT", self.root),
            patch.dict(os.environ, self.environment),
            patch.object(
                release.platform,
                "machine",
                return_value=self.environment.get("FAKE_MACHINE", "x86_64"),
            ),
            patch.object(release.platform, "system", return_value="Linux"),
        ):
            release.batch(self.args)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_manifest_inventory_and_selection_cover_all_native_packages(self):
        available = release.packages(self.root)
        self.assertEqual(set(available), {"net/http", "image/magick"})
        self.assertEqual(set(release.selected("all", available)), set(available))
        self.assertEqual(
            release.selected("net/http,image/magick", available),
            ["net/http", "image/magick"],
        )
        for invalid in ["unknown", "net/http net/http", ""]:
            with self.assertRaises(ValueError):
                release.selected(invalid, available)
        self.assertIn("ImageMagick 7", release.SYSTEM["image/magick"])
        self.assertEqual(release.HOSTS["aarch64"]["runner"], "ubuntu-24.04-arm")

    def test_verified_existing_signed_attachment_skips_build_and_publish(self):
        self.args.packages = "all"
        self.environment["FAKE_EXISTING"] = "1"
        self.execute()
        self.assertFalse(
            any(
                args[:2] == ["native", "build"] or "--auth" in args
                for args in self.calls()
            )
        )
        receipt = json.loads((self.args.output / "receipt.json").read_text())
        self.assertEqual(
            [p["state"] for p in receipt["packages"]], ["verified-existing"] * 2
        )

    def test_missing_attachment_builds_and_publishes_with_existing_cli(self):
        self.execute()
        calls = self.calls()
        build = next(args for args in calls if args[:2] == ["native", "build"])
        self.assertIn("x86_64-unknown-linux-gnu", build)
        publish = next(args for args in calls if "--auth" in args)
        self.assertEqual(publish[:3], ["publish", "--auth", "github-actions"])
        self.assertIn(build[-1], publish)

    def test_aarch64_uses_the_native_arm_runner_target(self):
        self.args.host = "aarch64"
        self.environment["FAKE_MACHINE"] = "aarch64"
        self.execute()
        build = next(args for args in self.calls() if args[:2] == ["native", "build"])
        self.assertIn("aarch64-unknown-linux-gnu", build)

    def test_build_only_has_no_publication_effect(self):
        self.args.publish = False
        self.execute()
        self.assertFalse(any("--auth" in args for args in self.calls()))

    def test_wrong_official_version_refuses_before_registry_or_build(self):
        self.environment["FAKE_VERSION"] = "0.6.3"
        with self.assertRaisesRegex(ValueError, "version mismatch"):
            self.execute()
        self.assertEqual(self.calls(), [["--version"]])

    def test_failed_signature_install_or_changed_source_never_builds(self):
        for flag in ["FAKE_INSTALL_FAILED", "FAKE_SOURCE_CHANGED"]:
            with self.subTest(flag=flag):
                self.environment = {"FAKE_LOG": str(self.log), flag: "1"}
                with self.assertRaises(subprocess.CalledProcessError):
                    self.execute()
                self.assertFalse(
                    any(args[:2] == ["native", "build"] for args in self.calls())
                )

    def test_wrong_verified_identity_is_not_a_skip(self):
        self.environment |= {
            "FAKE_EXISTING": "1",
            "FAKE_TARGET": "aarch64-unknown-linux-gnu",
        }
        with self.assertRaisesRegex(ValueError, "identity"):
            self.execute()
        self.assertFalse(any(args[:2] == ["native", "build"] for args in self.calls()))


if __name__ == "__main__":
    unittest.main()
