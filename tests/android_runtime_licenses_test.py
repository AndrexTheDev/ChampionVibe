"""Exercise the Android runtime license inventory using fixture publisher metadata."""

import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "runtime_licenses", Path(__file__).resolve().parents[1] / "scripts/report-android-runtime-licenses.py"
)
report = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(report)


class AndroidRuntimeLicensesTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.index = self.root / "modules.tsv"
        (self.root / "poms").mkdir()

    def test_reports_direct_missing_and_inherited_declarations(self):
        self.index.write_text("x\tz\t1\t\nx\tb\t2\t1.pom\nx\ta\t3\t0.pom\nx\tc\t1\t2.pom\n")
        exact = b'<project xmlns="http://maven.apache.org/POM/4.0.0"><licenses><license><name> MIT License </name><url>https://example.test/mit</url></license></licenses></project>'
        (self.root / "poms/0.pom").write_bytes(exact)
        (self.root / "poms/1.pom").write_text("<project><parent><artifactId>parent</artifactId></parent></project>")
        (self.root / "poms/2.pom").write_text("<project><licenses><license><name>${license}</name></license></licenses></project>")
        entries = report.collect(self.root)["modules"]
        self.assertEqual([entry["coordinates"] for entry in entries], ["x:a:3", "x:b:2", "x:c:1", "x:z:1"])
        self.assertEqual([entry["licenseStatus"] for entry in entries], [
            "declared", "no-usable-direct-declaration", "no-usable-direct-declaration", "pom-unavailable",
        ])
        self.assertEqual(entries[0]["licenses"], [{"name": "MIT License", "url": "https://example.test/mit"}])
        self.assertEqual(entries[0]["pomSha256"], hashlib.sha256(exact).hexdigest())
        self.assertIsNone(entries[3]["pomSha256"])

    def test_documented_upstream_evidence_requires_exact_module_and_parent(self):
        self.index.write_text("com.google.guava\tlistenablefuture\t1.0\t0.pom\n")
        pom = self.root / "poms/0.pom"
        exact = "<project><parent><groupId>com.google.guava</groupId><artifactId>guava-parent</artifactId><version>26.0-android</version></parent></project>"
        pom.write_text(exact)
        item = report.collect(self.root)["modules"][0]
        self.assertEqual(item["licenseStatus"], "upstream-source-documented")
        self.assertEqual(item["upstreamEvidence"]["license"], "Apache-2.0")
        pom.write_text(exact.replace("26.0-android", "27.0-android"))
        self.assertEqual(report.collect(self.root)["modules"][0]["licenseStatus"], "no-usable-direct-declaration")

    def test_rejects_empty_duplicate_and_out_of_directory_inventory(self):
        for text in ("", "x\tx\t1\t\nx\tx\t1\t\n", "x\tx\t1\t../other.pom\n"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                self.index.write_text(text)
                report.collect(self.root)


if __name__ == "__main__":
    unittest.main()
