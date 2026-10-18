"""Falsify the contract verifier, not the native backends: python3 -m unittest
discover -s scripts -p test_platform_contracts.py. Native runs remain required.
"""

import copy
import array
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("contracts", Path(__file__).with_name("platform-contracts.py"))
contracts = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contracts)


class Verification(unittest.TestCase):
    def setUp(self):
        self.receipt = dict(mode="normal", width=320, height=180, fps=30,
                            duration_seconds=.1, expected_frames=3)
        self.info = {"streams": [dict(index=0, codec_type="video", codec_name="h264", width=320,
                                     height=180, time_base="1/90000")]
                     + [dict(index=i, codec_type="audio", codec_name="aac", sample_rate="48000",
                             channels=2, time_base="1/48000") for i in (1, 2)],
                     "packets": [dict(stream_index=0, pts=i * 3000, duration=3000) for i in range(3)]
                     + [dict(stream_index=i, pts=0, duration=4800) for i in (1, 2)]}

    def test_real_accounting_and_timestamp_assertions(self):
        self.assertEqual(contracts.check_movie(self.info, self.receipt)["frames"], 3)
        for mutate in (
            lambda x: x["packets"].pop(0),
            lambda x: x["packets"][1].update(pts=0),
            lambda x: x["packets"][0].update(pts=1),
            lambda x: x["streams"][0].update(width=640),
            lambda x: x["streams"][1].update(sample_rate="44100"),
            lambda x: x["packets"][-1].update(duration=100),
            lambda x: x["streams"].pop(),
        ):
            broken = copy.deepcopy(self.info)
            mutate(broken)
            with self.assertRaises(contracts.Failure):
                contracts.check_movie(broken, self.receipt)

    def test_commit_is_checked_per_track_not_at_the_longest_end(self):
        receipt = dict(self.receipt, mode="commit")
        self.info["packets"][-1]["duration"] = 2400
        with self.assertRaises(contracts.Failure):
            contracts.check_movie(self.info, receipt, committed=.09)

    def test_present_packets_do_not_hide_a_shortened_edit_list(self):
        self.info["streams"][2].update(start_time="0", duration="0.04")
        with self.assertRaises(contracts.Failure):
            contracts.check_movie(self.info, dict(self.receipt, mode="commit"), committed=.09)

    def test_recovery_cannot_keep_a_fragment_timestamp_shift(self):
        for p in self.info["packets"][:3]:
            p["pts"] += 3960
        with self.assertRaises(contracts.Failure):
            contracts.check_movie(self.info, dict(self.receipt, mode="commit"), committed=.09)

    def test_pressure_timestamps_and_unreported_audio_loss_are_checked(self):
        receipt = dict(self.receipt, mode="pressure", audio_refused=[0, 0], audio_lost_tracks=0)
        for mutate in (lambda info: info["packets"][0].update(pts=1),
                       lambda info: info["packets"][2].update(pts=9000),
                       lambda info: info["packets"][-1].update(duration=100)):
            broken = copy.deepcopy(self.info)
            mutate(broken)
            with self.assertRaises(contracts.Failure):
                contracts.check_movie(broken, receipt)

    def test_reordered_packets_can_still_have_the_exact_presentation_grid(self):
        self.info["packets"][0], self.info["packets"][1] = self.info["packets"][1], self.info["packets"][0]
        self.assertEqual(contracts.check_movie(self.info, self.receipt)["frames"], 3)

    def test_decoded_sync_requires_real_on_off_pulses(self):
        receipt = dict(self.receipt, expected_frames=90, duration_seconds=3)
        video = bytes(255 if n % 30 < 3 else 0 for n in range(90))
        def pcm(shift=0, constant=False):
            samples = array.array("f", (.25 if constant or (n - shift) % 48000 < 4800 else 0 for n in range(144000)))
            if sys.byteorder != "little":
                samples.byteswap()
            return samples.tobytes()
        sound = pcm()
        with patch.object(contracts, "command", side_effect=[video, sound, sound]):
            self.assertEqual(contracts.av_sync(Path("unused"), receipt)["audio_minus_video_seconds"], [0, 0, 0, 0])
        for picture, audio in ((bytes([255] * 90), sound), (video, pcm(constant=True)), (video, pcm(2400))):
            with patch.object(contracts, "command", side_effect=[picture, audio, audio]):
                with self.assertRaises(contracts.Failure):
                    contracts.av_sync(Path("unused"), receipt)
        committed = dict(receipt, expected_frames=30, duration_seconds=1)
        with patch.object(contracts, "command", side_effect=[video[:30], sound[:192000], sound[:192000]]):
            self.assertEqual(contracts.av_sync(Path("unused"), committed, seconds=(0,))["audio_minus_video_seconds"], [0, 0])
        shifted = pcm(2112)[:192000]
        with patch.object(contracts, "command", side_effect=[video[:30], shifted, shifted]):
            with self.assertRaises(contracts.Failure):
                contracts.av_sync(Path("unused"), committed, seconds=(0,))

    def test_unavailable_and_omitted_cases_never_qualify_native(self):
        cases = [dict(case=c, status="passed") for c in contracts.CASES]
        report, code = contracts.summarize("native", "full", cases)
        self.assertEqual(code, 0)
        self.assertTrue(report["native_full_qualification"])
        for status, expected in (("unavailable", 2), ("failed", 1), ("not_requested", 2)):
            altered = copy.deepcopy(cases)
            altered[-1]["status"] = status
            report, code = contracts.summarize("native", "full", altered)
            self.assertEqual(code, expected)
            self.assertFalse(report["native_full_qualification"])
        report, code = contracts.summarize("native", "full", cases[:-1])
        self.assertEqual(code, 2)
        self.assertFalse(report["native_full_qualification"])
        duplicate = cases[:-1] + [cases[0]]
        self.assertEqual(contracts.summarize("native", "full", duplicate)[1], 2)

    def test_stub_and_core_passes_are_not_full_native_qualification(self):
        cases = [dict(case=c, status="passed") for c in contracts.CASES]
        for backend, suite in (("stub", "core"), ("native", "core"), ("stub", "full")):
            report, code = contracts.summarize(backend, suite, cases)
            self.assertEqual(code, 0)
            self.assertFalse(report["native_full_qualification"])

    def test_external_commands_have_real_failure_and_timeout_boundaries(self):
        with self.assertRaises(contracts.Failure):
            contracts.command([sys.executable, "-c", "raise SystemExit(17)"])
        with self.assertRaises(subprocess.TimeoutExpired):
            contracts.command([sys.executable, "-c", "import time; time.sleep(30)"], timeout=.1)

    def test_checkpoint_workers_are_killed_reaped_or_report_unavailable(self):
        real_popen = subprocess.Popen
        children = []
        programs = [("import time; print('{\"checkpoint\": {\"committed_seconds\": 1}}', flush=True); time.sleep(30)", "checkpoint"),
                    ("print('{\"result\": {\"unavailable\": \"missing\"}}', flush=True)", "unavailable"),
                    ("import time; time.sleep(30)", "timeout")]
        with tempfile.TemporaryDirectory() as temporary:
            for program, outcome in programs:
                def launch(_args, **kwargs):
                    child = real_popen([sys.executable, "-c", program], **kwargs)
                    children.append(child)
                    return child
                with patch.object(contracts.subprocess, "Popen", side_effect=launch):
                    if outcome == "timeout":
                        with self.assertRaises(contracts.Failure):
                            contracts.worker(Path(sys.executable), "stub", "commit", Path(temporary) / outcome, timeout=.1, kill_at_checkpoint=True)
                    else:
                        result = contracts.worker(Path(sys.executable), "stub", "commit", Path(temporary) / outcome, timeout=5, kill_at_checkpoint=True)
                        self.assertIn("committed_seconds" if outcome == "checkpoint" else "unavailable", result)
                self.assertIsNotNone(children[-1].poll())

    def test_interrupt_outside_a_case_still_reports_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder = Path(temporary) / "run"
            argv = ["contracts", "--bin", sys.executable, "--work", str(folder), "--backend", "stub"]
            with patch.object(sys, "argv", argv), patch.object(contracts, "run_cases", side_effect=KeyboardInterrupt):
                self.assertEqual(contracts.main(), 1)
            report = json.loads((folder / "report.json").read_text())
            self.assertFalse(report["suite_passed"])
            self.assertEqual(report["results"][0]["case"], "runner")


if __name__ == "__main__":
    unittest.main()
