import json
from pathlib import Path
import sys
import unittest


class TraceDashboardTest(unittest.TestCase):
    def test_layout_is_non_overlapping_and_preserves_units(self):
        for path in sys.argv[1:3]:
            dashboard = json.loads(Path(path).read_text())
            occupied = set()
            ids = set()
            for panel in dashboard["panels"]:
                self.assertNotIn(panel["id"], ids)
                ids.add(panel["id"])
                pos = panel["gridPos"]
                self.assertLessEqual(pos["x"] + pos["w"], 24)
                for x in range(pos["x"], pos["x"] + pos["w"]):
                    for y in range(pos["y"], pos["y"] + pos["h"]):
                        self.assertNotIn((x, y), occupied)
                        occupied.add((x, y))
        trace = json.loads(Path(sys.argv[1]).read_text())
        table = next(p for p in trace["panels"] if p["type"] == "table")
        duration = next(o for o in table["fieldConfig"]["overrides"] if o["matcher"]["options"] == "duration")
        # Jaeger returns microseconds; do not re-label the raw numbers as milliseconds.
        self.assertNotIn("unit", {p["id"] for p in duration["properties"]})
        self.assertEqual(table["options"]["cellHeight"], "md")
        overview = json.loads(Path(sys.argv[2]).read_text())
        original_panels = {p["id"]: p for p in overview["panels"] if p["id"] in range(1, 7)}
        self.assertEqual(len(original_panels), 6)
        self.assertEqual(original_panels[6]["type"], "state-timeline")
        self.assertTrue(all(p["targets"][0].get("expr") for p in original_panels.values()))

    def test_search_variables_and_native_trace_links(self):
        dashboard = json.loads(Path(sys.argv[1]).read_text())
        self.assertEqual(dashboard["uid"], "xscope-traces")
        self.assertEqual(dashboard["refresh"], "30s")
        variables = {v["name"]: v for v in dashboard["templating"]["list"]}
        self.assertEqual(variables["service"]["current"]["value"], "xscope-gateway")
        self.assertEqual(variables["min_duration"]["current"]["value"], "0ms")
        table = next(p for p in dashboard["panels"] if p["type"] == "table")
        target = table["targets"][0]
        self.assertEqual(target["datasource"]["uid"], "xscope-jaeger")
        self.assertEqual(target["queryType"], "search")
        self.assertEqual(target["limit"], 100)
        for name in ["service", "operation", "tags", "min_duration"]:
            field = "minDuration" if name == "min_duration" else name
            self.assertEqual(target[field], "$" + name)
        self.assertNotIn("query", target, "Search must not be pinned to a single trace ID")
        # Preserve the Jaeger plugin's built-in Trace ID -> Explore data link.
        for override in table["fieldConfig"]["overrides"]:
            self.assertNotIn("links", {p["id"] for p in override["properties"]})
        overview = json.loads(Path(sys.argv[2]).read_text())
        self.assertTrue(any("xscope-traces" in link.get("url", "") and link.get("keepTime") for link in overview["links"]))
        self.assertTrue(any("xscope-overview" in link.get("url", "") for link in dashboard["links"]))


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
