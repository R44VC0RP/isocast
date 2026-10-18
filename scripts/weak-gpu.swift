// Offscreen, bounded Metal contention for scripts/weak-machine.py (#110).
// Compile only on the test host, under hw-batch: swiftc -O weak-gpu.swift -o weak-gpu
import Foundation
import Metal

let seconds = min(600.0, max(1.0, Double(CommandLine.arguments.dropFirst().first ?? "30") ?? 30))
guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue() else {
    fatalError("Metal unavailable")
}
let source = """
#include <metal_stdlib>
using namespace metal;
kernel void contend(device float *out [[buffer(0)]], uint i [[thread_position_in_grid]]) {
    float x = float(i) * 0.000001f + 0.1f;
    for (uint k = 0; k < 1024; k++) { x = fract(sin(x + float(k)) * 1.17f); }
    out[i] = x;
}
"""
let library = try device.makeLibrary(source: source, options: nil)
let pipeline = try device.makeComputePipelineState(function: library.makeFunction(name: "contend")!)
// 4 MiB, one command buffer in flight. No growing queue or memory pressure.
let count = 1024 * 1024
let buffer = device.makeBuffer(length: count * 4, options: .storageModePrivate)!
let until = ProcessInfo.processInfo.systemUptime + seconds
while ProcessInfo.processInfo.systemUptime < until {
    autoreleasepool {
        let command = queue.makeCommandBuffer()!
        let encoder = command.makeComputeCommandEncoder()!
        encoder.setComputePipelineState(pipeline)
        encoder.setBuffer(buffer, offset: 0, index: 0)
        encoder.dispatchThreads(MTLSize(width: count, height: 1, depth: 1),
                                threadsPerThreadgroup: MTLSize(width: pipeline.threadExecutionWidth, height: 1, depth: 1))
        encoder.endEncoding()
        command.commit()
        command.waitUntilCompleted()
        if command.status == .error { fatalError("GPU command failed") }
    }
}
