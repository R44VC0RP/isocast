import CoreMediaIO
import Foundation

let source = CameraProvider()
CMIOExtensionProvider.startService(provider: source.provider)
CFRunLoopRun()
