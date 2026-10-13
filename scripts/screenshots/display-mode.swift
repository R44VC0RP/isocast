// Prints the main display's mode, or switches it:
//   swift display-mode.swift              current: WIDTHxHEIGHT@SCALE id
//   swift display-mode.swift ID           switch to mode ID (for this session)
//   swift display-mode.swift --hidpi      switch to the largest 2x mode
import CoreGraphics
import Foundation

let display = CGMainDisplayID()
let options = [kCGDisplayShowDuplicateLowResolutionModes: kCFBooleanTrue] as CFDictionary
let modes = (CGDisplayCopyAllDisplayModes(display, options) as? [CGDisplayMode]) ?? []

func describe(_ mode: CGDisplayMode) -> String {
    "\(mode.width)x\(mode.height)@\(mode.pixelWidth / max(mode.width, 1)) \(mode.ioDisplayModeID)"
}

func switchTo(_ mode: CGDisplayMode) {
    var config: CGDisplayConfigRef?
    CGBeginDisplayConfiguration(&config)
    CGConfigureDisplayWithDisplayMode(config, display, mode, nil)
    let error = CGCompleteDisplayConfiguration(config, .forSession)
    guard error == .success else {
        FileHandle.standardError.write("display-mode: switching failed (\(error.rawValue))\n".data(using: .utf8)!)
        exit(1)
    }
}

let args = CommandLine.arguments.dropFirst()
if let arg = args.first {
    let chosen: CGDisplayMode?
    if arg == "--hidpi" {
        chosen = modes.filter { $0.isUsableForDesktopGUI() && $0.pixelWidth == 2 * $0.width }
            .max { $0.width * $0.height < $1.width * $1.height }
    } else {
        chosen = modes.first { String($0.ioDisplayModeID) == arg && $0.isUsableForDesktopGUI() }
    }
    guard let mode = chosen else {
        FileHandle.standardError.write("display-mode: no such mode \(arg)\n".data(using: .utf8)!)
        exit(1)
    }
    switchTo(mode)
    sleep(2)
}
if let current = CGDisplayCopyDisplayMode(display) {
    print(describe(current))
}
