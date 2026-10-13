// Moves the pointer out of the way (to the menu bar's right end), so it isn't
// in the screenshots: swift cursor.swift
import CoreGraphics
let bounds = CGDisplayBounds(CGMainDisplayID())
CGWarpMouseCursorPosition(CGPoint(x: bounds.maxX - 24, y: 8))
