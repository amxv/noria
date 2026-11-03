"""Native release metadata tests requiring only Python's standard library."""
from pathlib import Path
import json
import io
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
import release


class ReleaseTests(unittest.TestCase):
    def test_release_version_matches_tag(self):
        self.assertEqual(release.check("v" + release.VERSION), release.VERSION)
        for tag in ("v0.0.0", "0.3.0", "v2.0.0", "../escape"):
            if tag != "v" + release.VERSION:
                with self.assertRaises(ValueError):
                    release.check(tag)

    def test_release_notes_are_maintainer_written(self):
        result = release.notes("v" + release.VERSION)
        self.assertIn("noria upgrade", result)
        self.assertIn("install.sh", result)
        self.assertIn("install.ps1", result)
        self.assertNotIn("npm install", result)

    def test_native_installers_are_mirrored_for_docs(self):
        self.assertEqual((ROOT / "scripts/install.sh").read_bytes(),
                         (ROOT / "docs/public/install.sh").read_bytes())
        self.assertEqual((ROOT / "scripts/install.ps1").read_bytes(),
                         (ROOT / "docs/public/install.ps1").read_bytes())

    def test_tray_starts_at_install_and_persists_at_login(self):
        shell = (ROOT / "scripts/install.sh").read_text()
        windows = (ROOT / "scripts/install.ps1").read_text()
        for content in (shell, windows):
            self.assertIn("preferences tray-at-login on", content)
            self.assertIn("tray open", content)
            self.assertIn("NORIA_NO_LAUNCH", content)

    def test_node_and_npm_distribution_are_not_used(self):
        self.assertFalse((ROOT / "package.json").exists())
        self.assertFalse((ROOT / "bin/noria.js").exists())
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        for phrase in ("npm-publish", "npm install", "npm publish", "setup-node", "NODE_AUTH_TOKEN"):
            self.assertNotIn(phrase, workflow)

    def test_docs_only_github_actions_deployment(self):
        """Unrelated code pushes must never request a Vercel deployment."""
        config = json.loads((ROOT / "docs/vercel.json").read_text())
        self.assertFalse(config["git"]["deploymentEnabled"])
        self.assertNotIn("ignoreCommand", config)
        workflow = (ROOT / ".github/workflows/docs-deploy.yml").read_text()
        for path in ('docs/**', 'scripts/install.sh', 'scripts/install.ps1'):
            self.assertIn(f"- '{path}'", workflow)
        for required in ("branches: [main]", "VERCEL_TOKEN", "vars.VERCEL_ORG_ID",
                         "vars.VERCEL_PROJECT_ID", "deploy --prebuilt --prod",
                         "bun run build", "scripts/prepare-prebuilt.mjs"):
            self.assertIn(required, workflow)
        self.assertNotIn('"**"', workflow)
        self.assertNotIn('"*"', workflow)
        self.assertFalse((ROOT / "docs/scripts/should-build.mjs").exists())

    def write_archive(self, path, system, extra=None, omit=None):
        files = {"LICENSE", "THIRD_PARTY_NOTICES.md", "THIRD_PARTY_LICENSES.txt"}
        files |= {"noria.exe", "noria-tray.exe"} if system == "windows" else {"noria"}
        if system == "linux":
            files.add("noria-tray")
        if system == "darwin":
            files.update(("Noria.app/Contents/MacOS/noria-menubar", "Noria.app/Contents/Info.plist", "Noria.app/Contents/_CodeSignature/CodeResources"))
        if omit:
            files.remove(omit)
        with tarfile.open(path, "w:gz") as archive:
            for name in sorted(files):
                info = tarfile.TarInfo(name)
                info.size, info.mode = 7, 0o755
                archive.addfile(info, io.BytesIO(b"fixture"))
            if extra:
                info = tarfile.TarInfo(extra)
                info.size = 7
                archive.addfile(info, io.BytesIO(b"fixture"))

    def test_complete_release_and_tampered_checksum(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            self.assertEqual(len(release.expected_artifacts()), 12)
            for name in release.expected_artifacts():
                if name.endswith(".tar.gz"):
                    self.write_archive(directory / name, name.split("_")[1])
                else:
                    (directory / name).write_bytes(b"fixture")
            release.checksums(directory)
            release.verify(directory)
            (directory / "noria_linux_arm64").write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
                release.verify(directory)

    def test_archive_rejects_missing_tray_and_unsafe_paths(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "archive.tar.gz"
            self.write_archive(path, "linux", omit="noria-tray")
            with self.assertRaisesRegex(ValueError, "Missing"):
                release.validate_archive(path, "linux")
            for extra in ("../outside", "/absolute", "noria", "windows\\escape"):
                self.write_archive(path, "linux", extra=extra)
                with self.assertRaises(ValueError):
                    release.validate_archive(path, "linux")


if __name__ == "__main__":
    unittest.main()
