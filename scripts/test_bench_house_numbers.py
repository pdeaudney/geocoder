import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("house_bench", Path(__file__).with_name("bench-house-numbers.py"))
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


class HouseBenchmarkTests(unittest.TestCase):
    def address(self, **changes):
        return {"country": "US", "number": "255-257", "unit": "Unit 3B", "street": "Jackson Street",
                "city": "Test City", "state": "Test State", "postcode": "12345", "lat": 40.0,
                "lon": -74.0, "stratum": "unit", **changes}

    def test_generated_requests_do_not_supply_answer_coordinates(self):
        address = self.address()
        self.assertTrue(bench.freeform(address).startswith("Unit 3B, 255-257 Jackson Street"))
        self.assertFalse("Unit Unit" in bench.freeform(address))
        cases = bench.make_cases([address])
        self.assertEqual({c["tags"][2] for c in cases}, {"structured", "freeform", "abbreviation", "typo"})
        for case in cases:
            self.assertFalse({"lat", "lon", "bias_lat", "bias_lng"} & case["request"]["query"].keys())
            self.assertEqual(len(case["expect"]["checks"]), len(bench.METRICS))
            for check in case["expect"]["checks"]:
                self.assertEqual(check["fields"]["address.house_number"], "255-257")
                self.assertEqual(check["fields"]["address.unit"], "Unit 3B")
                if "coordinates" in check:
                    self.assertIn(check["coordinates"]["max_m"], bench.RADII)

    def test_missing_response_counts_as_failure_and_street_is_not_an_address(self):
        cases = bench.make_cases([self.address(unit="", number="12A", stratum="suffix")])[:2]
        reports = {case["id"]: {"http_status": None, "check_results": [], "failures": ["timeout"]} for case in cases}
        reports[cases[0]["id"]] = {"http_status": 200, "check_results": [False] * len(bench.METRICS),
                                  "first_result": {"kind": 2, "address": {"house_number": None}}, "failures": []}
        summary, failures = bench.summarize(cases, reports)
        self.assertEqual(summary["all"]["total"], 2)
        self.assertEqual(summary["all"]["metrics"]["top1_50m"]["passed"], 0)
        self.assertEqual(summary["all"]["outcomes"]["street_fallback"], 1)
        self.assertEqual(summary["all"]["outcomes"]["http_errors"], 1)
        self.assertEqual(len(failures), 2)
        reports[cases[1]["id"]] = {"http_status": 500, "check_results": [True] * len(bench.METRICS), "failures": ["HTTP 500"]}
        summary, failures = bench.summarize(cases, reports)
        self.assertEqual(summary["all"]["metrics"]["top1_50m"]["passed"], 0)
        self.assertEqual(len(failures), 2)


if __name__ == "__main__":
    unittest.main()
