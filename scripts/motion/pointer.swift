// The motion harness's real pointer (scripts/motion/window.py), on a test
// Mac only: it moves the real mouse.
//
//   swift pointer.swift windows OWNER       the owner's on-screen windows (* for all):
//                                           [{"id", "name", "x", "y", "w", "h"}]
//   swift pointer.swift replay PLAN OUT     post PLAN's events at their times
//
// PLAN is JSON: {"events": [{"t": seconds, "kind": "move|down|drag|up",
// "x": points, "y": points}]}, in global screen points (top-left origin),
// times from the start. OUT gets each event as posted with its host time
// (CLOCK_UPTIME_RAW, ns: the recordings' manifests' `host_time_ns` clock).
// Posting needs Accessibility for the process that runs it; without it,
// macOS drops the events silently, so that is checked first.
import ApplicationServices
import CoreGraphics
import Foundation

func fail(_ message: String) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(1)
}

let args = CommandLine.arguments
guard args.count >= 3 else { fail("usage: pointer.swift windows OWNER | replay PLAN OUT") }

func now() -> UInt64 { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }

switch args[1] {
case "windows":
    let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
        as? [[String: Any]] ?? []
    var out: [[String: Any]] = []
    for w in list where args[2] == "*" || (w[kCGWindowOwnerName as String] as? String) == args[2] {
        guard let b = w[kCGWindowBounds as String] as? [String: CGFloat] else { continue }
        out.append([
            "id": w[kCGWindowNumber as String] as? Int ?? 0,
            "owner": w[kCGWindowOwnerName as String] as? String ?? "",
            "name": w[kCGWindowName as String] as? String ?? "",
            "layer": w[kCGWindowLayer as String] as? Int ?? 0,
            "x": b["X"] ?? 0, "y": b["Y"] ?? 0, "w": b["Width"] ?? 0, "h": b["Height"] ?? 0,
        ])
    }
    let data = try! JSONSerialization.data(withJSONObject: out)
    print(String(data: data, encoding: .utf8)!)

case "replay":
    guard args.count >= 4 else { fail("usage: pointer.swift replay PLAN OUT") }
    guard AXIsProcessTrusted() else {
        fail("pointer.swift: this process may not post events (System Settings › Privacy & Security › Accessibility)")
    }
    let plan = try! JSONSerialization.jsonObject(with: Data(contentsOf: URL(fileURLWithPath: args[2]))) as! [String: Any]
    let events = plan["events"] as! [[String: Any]]
    let source = CGEventSource(stateID: .hidSystemState)
    var log: [[String: Any]] = []
    let start = now() + 200_000_000
    for e in events {
        let t = e["t"] as! Double
        let due = start + UInt64(max(0, t) * 1e9)
        // Sleep most of the way, then spin: posting on time matters more
        // than the CPU of a few seconds' run.
        while now() + 2_000_000 < due { usleep(500) }
        while now() < due {}
        let p = CGPoint(x: e["x"] as! Double, y: e["y"] as! Double)
        let type: CGEventType
        switch e["kind"] as! String {
        case "down": type = .leftMouseDown
        case "up": type = .leftMouseUp
        case "drag": type = .leftMouseDragged
        default: type = .mouseMoved
        }
        guard let event = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: p, mouseButton: .left) else {
            fail("pointer.swift: couldn't make a \(type) event")
        }
        event.post(tap: .cghidEventTap)
        log.append(["t": t, "host_ns": now(), "kind": e["kind"]!, "x": p.x, "y": p.y])
    }
    let data = try! JSONSerialization.data(withJSONObject: ["events": log])
    // Atomic: a new file renamed into place, never written through a link.
    try! data.write(to: URL(fileURLWithPath: args[3]), options: .atomic)

default:
    fail("usage: pointer.swift windows OWNER | replay PLAN OUT")
}
