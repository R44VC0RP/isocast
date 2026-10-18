#!/usr/bin/env python3
"""Run identical Platform contracts in isolated, bounded native workers.

Build: cargo build --release -p isocast-platform-contracts
Run on a test machine, never Ryan's Mac:
  python3 scripts/platform-contracts.py --bin BIN --work NEW_DIR --suite core
  python3 scripts/platform-contracts.py --bin BIN --work NEW_DIR --suite media --hardware
  python3 scripts/platform-contracts.py --bin BIN --work NEW_DIR --suite full --hardware

core: host clock and bounded BGRA pools. media adds GPU completion/lifetime,
native recording, encoder-loss accounting, ownership/cancel and killed-writer
recovery. full also captures the main display briefly, ONLY with an already
granted permission; it does not prompt, save screen pixels or change settings.
Media/full require ffmpeg+ffprobe. Acquire the shared hardware/timing lock in
the machine's normal runner (hw-batch on Mac; gpu.lock on exon-desk).

--backend stub is an explicit CPU-placeholder baseline, never native parity.
Missing native implementations/permissions are UNAVAILABLE, not passes.
Exit 0: all requested contracts passed; 1: failure; 2: unavailable or usage.
Every run preserves report.json, per-worker logs and generated media. Reuse
of a work folder is refused; no existing files or default daemon are touched.
"""

import argparse
import array
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
from fractions import Fraction

CASES = ("clock", "pools", "gpu", "normal", "pressure", "ownership", "recovery", "capture")
SUITES = {"core": CASES[:2], "media": CASES[:-1], "full": CASES}


class Failure(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Failure(message)


def stop(child):
    if child.poll() is None:
        if os.name == "posix":
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        else:
            # Workers and ffmpeg/ffprobe do not spawn child processes.
            child.kill()
    child.wait(timeout=10)


def command(args, timeout=30):
    child = subprocess.Popen([str(a) for a in args], stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, start_new_session=os.name == "posix")
    try:
        stdout, stderr = child.communicate(timeout=timeout)
    except BaseException:
        stop(child)
        raise
    finally:
        child.stdout.close()
        child.stderr.close()
    require(child.returncode == 0, f"{args[0]} exit {child.returncode}: {stderr.decode(errors='replace')[-3000:]}")
    return stdout


def worker(binary, backend, case, folder, timeout=45, kill_at_checkpoint=False):
    # The parent creates each directory and marker before a worker can write.
    folder.mkdir(exist_ok=False)
    (folder / ".contract-owner").write_bytes(b"isocast-platform-contracts\n")
    return worker_in(binary, backend, case, folder, timeout, kill_at_checkpoint)


def worker_in(binary, backend, case, folder, timeout=45, kill_at_checkpoint=False):
    env = {k: v for k, v in os.environ.items() if not k.startswith("ISOCAST_")}
    env["ISOCAST_CRASH_SAFE"] = "1"  # Child only; never changes installed defaults.
    stdout_path, stderr_path = folder / f"{case}.out", folder / f"{case}.err"
    with stdout_path.open("xb") as out, stderr_path.open("xb") as err:
        child = subprocess.Popen([str(binary), backend, case, str(folder)], env=env,
                                 stdout=out, stderr=err, start_new_session=os.name == "posix")
        try:
            if kill_at_checkpoint:
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    data = stdout_path.read_bytes()
                    require(len(data) < 1 << 20, "worker checkpoint exceeds 1 MiB")
                    if data.endswith(b"\n"):
                        value = json.loads(data)
                        if "checkpoint" in value:
                            require(child.poll() is None, "writer exited before checkpoint kill")
                            stop(child)
                            return value["checkpoint"]
                        if value.get("result", {}).get("unavailable"):
                            child.wait(timeout=10)
                            return value["result"]
                        raise Failure("commit worker returned without a live checkpoint")
                    if child.poll() is not None:
                        # The earlier read can race the final flushed line of
                        # a worker that reports unavailable and exits at once.
                        if stdout_path.read_bytes() != data:
                            continue
                        raise Failure(f"commit worker exited: {stderr_path.read_text(encoding='utf-8', errors='replace')[-3000:]}")
                    time.sleep(.005)
                raise Failure("timed out waiting for the writer's commit receipt")
            child.wait(timeout=timeout)
            require(child.returncode == 0, f"worker {case} exit {child.returncode}: {stderr_path.read_text(encoding='utf-8', errors='replace')[-3000:]}")
        finally:
            stop(child)
    value = json.loads(stdout_path.read_text(encoding="utf-8"))
    require("result" in value, "worker returned no result")
    return value["result"]


def probe(path):
    return json.loads(command(["ffprobe", "-v", "error", "-show_streams", "-show_packets", "-of", "json", path]))


def packet_time(packet, stream):
    return int(packet["pts"]) * Fraction(stream["time_base"])


def stream_ends(info):
    # Use packet times, including negative AAC pre-roll, rather than guessed
    # frame rates or a container's maximum duration for every track.
    ends = {}
    for stream in info["streams"]:
        packets = [p for p in info["packets"] if p["stream_index"] == stream["index"]]
        require(packets, f"stream {stream['index']} has no packets")
        require(all(int(p["duration"]) > 0 for p in packets), "nonpositive packet duration")
        end = max(packet_time(p, stream) + int(p["duration"]) * Fraction(stream["time_base"]) for p in packets)
        # A final edit list may hide otherwise-present packets. Preserving
        # bytes does not prove a commitment if the playable track was trimmed.
        if stream.get("duration") not in (None, "N/A"):
            playable_end = Fraction(stream.get("start_time", "0")) + Fraction(stream["duration"])
            end = min(end, playable_end)
        ends[stream["index"]] = end
    return ends


def check_movie(info, receipt, committed=None, finalized=True):
    streams = info["streams"]
    require([s["codec_type"] for s in streams] == ["video", "audio", "audio"], "expected one video and two AAC tracks")
    require(streams[0]["codec_name"] == "h264" and all(s["codec_name"] == "aac" for s in streams[1:]), "wrong native codecs")
    require(all(int(s["sample_rate"]) == 48000 and s["channels"] == 2 for s in streams[1:]), "wrong audio format")
    require((streams[0]["width"], streams[0]["height"]) == (receipt["width"], receipt["height"]), "wrong picture size")
    video = [p for p in info["packets"] if p["stream_index"] == streams[0]["index"]]
    require(len(video) == receipt["expected_frames"], f"stored {len(video)} frames != accounted {receipt['expected_frames']}")
    # FileWriter does not forbid B-frames. Check the presentation grid without
    # confusing decode-order packet delivery with presentation order.
    times = sorted(packet_time(p, streams[0]) for p in video)
    require(len(set(times)) == len(times), "video presentation timestamps repeat")
    if finalized:
        require(all((t * receipt["fps"]).denominator == 1 and 0 <= t < Fraction(str(receipt["duration_seconds"])) for t in times),
                "stored video timestamps are off-grid or outside the delivered timeline")
        if receipt["mode"] in ("normal", "commit") and receipt.get("refused", 0) == 0 and receipt.get("encoder_lost", 0) == 0:
            require(times == [Fraction(i, receipt["fps"]) for i in range(len(video))], "pictures are not on their saved-clock ticks")
    ends = stream_ends(info)
    if receipt["mode"] == "normal":
        for end in ends.values():
            require(abs(float(end) - receipt["duration_seconds"]) <= 1024 / 48000 + 1e-6, "track end differs from the delivered timeline")
    if receipt["mode"] == "pressure":
        require(all(float(end) <= receipt["duration_seconds"] + 1024 / 48000 + 1e-6 for end in ends.values()), "pressure output extends beyond the requested end")
        if receipt.get("audio_refused") == [0, 0] and receipt.get("audio_lost_tracks") == 0:
            require(all(abs(float(ends[s["index"]]) - receipt["duration_seconds"]) <= 1024 / 48000 + 1e-6 for s in streams[1:]), "pressure output lost unreported audio")
    if committed is not None:
        require(all(float(end) + 1e-6 >= committed for end in ends.values()), "recovery shortened the advertised common commit")
    return {"frames": len(video), "track_ends_seconds": {str(k): float(v) for k, v in ends.items()}}


def full_decode(path):
    command(["ffmpeg", "-nostdin", "-v", "error", "-xerror", "-hwaccel", "none", "-i", path, "-map", "0", "-f", "null", "-"], 45)


def av_sync(path, receipt, seconds=(1, 2)):
    video = command(["ffmpeg", "-nostdin", "-v", "error", "-i", path, "-map", "0:v:0",
                     "-vf", "crop=iw:ih/4:0:3*ih/4,scale=1:1,format=gray", "-frames:v", "120",
                     "-fps_mode", "passthrough", "-f", "rawvideo", "-"])
    require(len(video) == receipt["expected_frames"], "decoded video count disagrees")
    for second in seconds:
        require((second == 0 or video[int((second - .05) * receipt["fps"])] < 60)
                and video[int((second + .15) * receipt["fps"])] < 60
                and video[int((second + .05) * receipt["fps"])] > 200,
                f"video does not contain the expected on/off pulse at {second}s")
    offsets = []
    for track in range(2):
        audio = array.array("f", command(["ffmpeg", "-nostdin", "-v", "error", "-i", path,
                                         "-map", f"0:a:{track}", "-t", "4", "-ac", "1", "-ar", "48000", "-f", "f32le", "-"]))
        if sys.byteorder != "little":
            audio.byteswap()
        require(len(audio) >= int((max(seconds) + .2) * 48000), "decoded audio ends before the last pulse")
        for second in seconds:
            for offset, quiet in ((-.05, True), (.05, False), (.15, True)):
                if second + offset < 0:
                    continue
                at = int((second + offset) * 48000)
                rms = math.sqrt(sum(x * x for x in audio[at:at + 480]) / 480)
                require(rms < .02 if quiet else rms > .08,
                        f"audio track {track} has no expected {'silence' if quiet else 'tone'} near {second + offset}s")
            first = max(0, int((second - .1) * receipt["fps"]))
            last = min(len(video), int((second + .15) * receipt["fps"]))
            flashes = [i / receipt["fps"] for i in range(first, last) if video[i] > 200]
            require(flashes, f"no decoded video flash at {second}s")
            beeps = []
            for at in range(max(0, int((second - .1) * 48000)), min(len(audio) - 48, int((second + .15) * 48000)), 48):
                rms = math.sqrt(sum(x * x for x in audio[at:at + 48]) / 48)
                if rms > .08:
                    beeps.append(at / 48000)
            require(beeps, f"no decoded audio pulse at {second}s on track {track}")
            offset = beeps[0] - flashes[0]
            require(abs(offset) <= 1 / receipt["fps"], f"A/V skew {offset}s exceeds one frame")
            offsets.append(offset)
    return {"audio_minus_video_seconds": offsets}


def payloads(path, info):
    digests = {}
    with path.open("rb") as file:
        for packet in info["packets"]:
            file.seek(int(packet["pos"]))
            size = int(packet["size"])
            data = file.read(size)
            require(len(data) == size, "truncated packet payload")
            digests.setdefault(packet["stream_index"], hashlib.sha256()).update(data)
    return {i: digest.hexdigest() for i, digest in digests.items()}


def recovery(binary, backend, folder):
    receipt = worker(binary, backend, "commit", folder, kill_at_checkpoint=True)
    if "unavailable" in receipt:
        return receipt
    require(receipt["committed_seconds"] > 0, "writer delivered no positive commit watermark")
    movie = folder / "take.mp4"
    before = probe(movie)
    # A commit has not called finish(), so it cannot guess finish's repeat
    # count. Measure the published packets before repair and require all back.
    measured_receipt = dict(receipt, expected_frames=sum(p["codec_type"] == "video" for p in before["packets"]))
    check_movie(before, measured_receipt, receipt["committed_seconds"], finalized=False)
    original = payloads(movie, before)
    manifest = {"version": 1, "setup": "Platform contract", "status": "recording",
                "started_at": "2026-01-01T00:00:00Z", "host_time_ns": 0, "duration_seconds": None,
                "files": [{"path": "take.mp4", "source": "pattern", "kind": "test_pattern",
                           "width": receipt["width"], "height": receipt["height"], "fps": receipt["fps"],
                           "audio": [{"source": "tone1", "channels": 2}, {"source": "tone2", "channels": 2}],
                           "start_offset_seconds": 0.0, "duration_seconds": None, "frames": None,
                           "repeated_frames": None, "dropped_frames": None, "audio_dropouts": None}],
                "errors": [], "pauses": [{"start_seconds": receipt["duration_seconds"], "end_seconds": None,
                                          "time_seconds": receipt["duration_seconds"], "recoverable_seconds_at_pause": receipt["committed_seconds"]}]}
    path = folder / "take.session.json"
    path.write_text(json.dumps(manifest), encoding="utf-8")
    result = worker_in(binary, backend, "recover", folder)
    require(len(result["sessions"]) == 1, "engine recovery did not report its orphan")
    after = probe(movie)
    require(payloads(movie, after) == original, "recovery dropped or changed complete packet payloads")
    verified = check_movie(after, measured_receipt, receipt["committed_seconds"])
    stored = json.loads(path.read_text(encoding="utf-8"))
    require(stored["status"] == "interrupted", "manifest still claims a live/complete take")
    require(stored["files"][0]["frames"] == verified["frames"], "manifest frames do not equal stored packets")
    end = max(verified["track_ends_seconds"].values())
    require(abs(stored["files"][0]["duration_seconds"] - end) <= 1 / receipt["fps"], "manifest duration does not match the movie")
    require(stored["pauses"] == manifest["pauses"], "recovery rewrote a pause commitment")
    full_decode(movie)
    recovered_sync = av_sync(movie, measured_receipt, seconds=(0,))
    movie_hash = hashlib.sha256(movie.read_bytes()).digest()
    manifest_bytes = path.read_bytes()
    # Separate receipt files, but the same exact media/manifest directory.
    (folder / "recover.out").rename(folder / "recover-first.out")
    (folder / "recover.err").rename(folder / "recover-first.err")
    worker_in(binary, backend, "recover", folder)
    require(movie_hash == hashlib.sha256(movie.read_bytes()).digest() and path.read_bytes() == manifest_bytes, "second recovery is not byte-idempotent")
    return {"receipt": receipt, "verified": verified, "sync": recovered_sync, "packet_payloads_preserved": True, "idempotent": True}


def summarize(backend, suite, results):
    selected = [r for r in results if r["case"] in SUITES[suite]]
    passed = (len(selected) == len(SUITES[suite])
              and {r["case"] for r in selected} == set(SUITES[suite])
              and all(r["status"] == "passed" for r in selected)
              and not any(r["status"] == "failed" for r in results))
    report = {"version": 1, "backend": backend, "suite": suite,
              "suite_passed": passed, "native_full_qualification": passed and backend == "native" and suite == "full",
              "results": results}
    code = 0 if passed else 1 if any(r["status"] == "failed" for r in results) else 2
    return report, code


def run_cases(args, binary, work, results):
    for case in CASES:
        row = {"case": case, "status": "not_requested"}
        if case in SUITES[args.suite]:
            started = time.monotonic()
            try:
                value = recovery(binary, args.backend, work / case) if case == "recovery" else worker(binary, args.backend, case, work / case)
                if "unavailable" in value:
                    row.update(status="unavailable", reason=value["unavailable"])
                else:
                    if case in ("normal", "pressure"):
                        movie = work / case / "take.mp4"
                        value["verified"] = check_movie(probe(movie), value)
                        full_decode(movie)
                        if case == "normal":
                            value["sync"] = av_sync(movie, value)
                    row.update(status="passed", evidence=value)
            except Exception as error:
                row.update(status="failed", error=str(error))
            row["elapsed_seconds"] = time.monotonic() - started
        results.append(row)
        print(json.dumps(row), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--bin", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--backend", choices=("native", "stub"), default="native")
    parser.add_argument("--suite", choices=SUITES, default="core")
    parser.add_argument("--hardware", action="store_true", help="explicitly permit GPU/encoder/capture cases on this test machine")
    args = parser.parse_args()
    if args.suite != "core" and not args.hardware:
        parser.error("media/full require --hardware and the test machine's normal reservation")
    binary = args.bin.resolve(strict=True)
    args.work.mkdir(parents=True, exist_ok=False)
    work = args.work.resolve()
    results = []
    def terminate(_signum, _frame):
        raise KeyboardInterrupt("contract run terminated")
    previous = signal.signal(signal.SIGTERM, terminate)
    try:
        try:
            run_cases(args, binary, work, results)
        except KeyboardInterrupt as error:
            results.append({"case": "runner", "status": "failed", "error": str(error) or "interrupted"})
        finally:
            report, code = summarize(args.backend, args.suite, results)
            partial = work / "report.json.partial"
            partial.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
            partial.replace(work / "report.json")
    finally:
        signal.signal(signal.SIGTERM, previous)
    return code


if __name__ == "__main__":
    sys.exit(main())
