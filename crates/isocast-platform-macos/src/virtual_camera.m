// Native camera transport. CoreMediaIO transfers the IOSurface reference and
// keeps it alive through the extension and its consumers: no pixel copies.
#import <Foundation/Foundation.h>
#import <CoreMediaIO/CoreMediaIO.h>
#import <CoreMediaIO/CMIOSampleBuffer.h>
#import <CoreVideo/CoreVideo.h>
#import <SystemExtensions/SystemExtensions.h>
#import <AVFoundation/AVFoundation.h>
#import <Security/Security.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <bsm/audit.h>

bool isocast_camera_auth_parent(int fd) {
    uid_t uid; gid_t gid;
    if (getpeereid(fd, &uid, &gid) || uid != geteuid()) return false;
    audit_token_t token;
    socklen_t size = sizeof(token);
    if (getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, &token, &size) || size != sizeof(token)) return false;
    CFDataRef audit = CFDataCreate(NULL, (const UInt8 *)&token, sizeof(token));
    const void *key = kSecGuestAttributeAudit, *value = audit;
    CFDictionaryRef attributes = CFDictionaryCreate(NULL, &key, &value, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    SecCodeRef peer = NULL, own = NULL; SecRequirementRef requirement = NULL;
    OSStatus status = SecCodeCopySelf(kSecCSDefaultFlags, &own);
    if (!status) status = SecCodeCopyDesignatedRequirement(own, kSecCSDefaultFlags, &requirement);
    if (!status) status = SecCodeCopyGuestWithAttributes(NULL, attributes, kSecCSDefaultFlags, &peer);
    if (!status) status = SecCodeCheckValidity(peer, kSecCSDefaultFlags, requirement);
    if (peer) CFRelease(peer); if (own) CFRelease(own); if (requirement) CFRelease(requirement);
    CFRelease(attributes); CFRelease(audit);
    return status == errSecSuccess;
}

#define CAMERA_UID CFSTR("D07A1962-6A48-4BD3-9074-DA9D25FC0141")

typedef struct {
    CMIODeviceID device;
    CMIOStreamID stream;
    CMSimpleQueueRef queue;
    uint32_t width, height, fps;
    uint64_t sequence;
} IsocastCamera;

static CMIOObjectPropertyAddress address(CMIOObjectPropertySelector selector) {
    return (CMIOObjectPropertyAddress){selector, kCMIOObjectPropertyScopeGlobal, kCMIOObjectPropertyElementMain};
}
static OSStatus get(CMIOObjectID object, CMIOObjectPropertySelector selector, UInt32 size, void *value) {
    CMIOObjectPropertyAddress a = address(selector);
    UInt32 used = 0;
    return CMIOObjectGetPropertyData(object, &a, 0, NULL, size, &used, value);
}
static OSStatus set(CMIOObjectID object, CMIOObjectPropertySelector selector, UInt32 size, const void *value) {
    CMIOObjectPropertyAddress a = address(selector);
    return CMIOObjectSetPropertyData(object, &a, 0, NULL, size, value);
}
static CMIOObjectID *objects(CMIOObjectID parent, CMIOObjectPropertySelector selector, UInt32 *count) {
    CMIOObjectPropertyAddress a = address(selector);
    UInt32 size = 0;
    *count = 0;
    if (CMIOObjectGetPropertyDataSize(parent, &a, 0, NULL, &size) || size > 65536) return NULL;
    CMIOObjectID *ids = calloc(1, size);
    if (ids && !get(parent, selector, size, ids)) *count = size / sizeof(*ids);
    return ids;
}
static void altered(CMIOStreamID stream, void *element, void *context) {
    (void)stream; (void)element; (void)context;
}

void *isocast_camera_open(uint32_t width, uint32_t height, uint32_t fps, int32_t *error, int32_t *stage) {
    *stage = 1;
    *error = -1; // Missing extension, distinct from native failures.
    IsocastCamera *camera = calloc(1, sizeof(*camera));
    if (!camera) { *error = -2; return NULL; }
    camera->width = width; camera->height = height; camera->fps = fps;
    UInt32 count = 0;
    CMIOObjectID *ids = objects(kCMIOObjectSystemObject, kCMIOHardwarePropertyDevices, &count);
    for (UInt32 i = 0; i < count; i++) {
        CFStringRef uid = NULL;
        if (!get(ids[i], kCMIODevicePropertyDeviceUID, sizeof(uid), &uid) && uid) {
            if (CFEqual(uid, CAMERA_UID)) camera->device = ids[i];
            CFRelease(uid);
        }
    }
    free(ids);
    if (!camera->device) { free(camera); return NULL; }
    *stage = 2;
    ids = objects(camera->device, kCMIODevicePropertyStreams, &count);
    for (UInt32 i = 0; i < count; i++) {
        UInt32 direction = 1;
        CFStringRef name = NULL;
        if (!get(ids[i], kCMIOStreamPropertyDirection, sizeof(direction), &direction) && direction == 0
            && !get(ids[i], kCMIOObjectPropertyName, sizeof(name), &name) && name) {
            if (CFEqual(name, CFSTR("Isocast Camera Sink"))) {
                if (camera->stream) { CFRelease(name); free(ids); free(camera); return NULL; }
                camera->stream = ids[i];
            }
            CFRelease(name);
        }
    }
    free(ids);
    if (!camera->stream) { free(camera); return NULL; }
    *stage = 3;
    CFArrayRef formats = NULL;
    *error = get(camera->stream, kCMIOStreamPropertyFormatDescriptions, sizeof(formats), &formats);
    if (!*error && !formats) *error = -3;
    CMFormatDescriptionRef selected = NULL;
    if (!*error && formats) {
        for (CFIndex i = 0; i < CFArrayGetCount(formats); i++) {
            CMFormatDescriptionRef format = (CMFormatDescriptionRef)CFArrayGetValueAtIndex(formats, i);
            CMVideoDimensions dims = CMVideoFormatDescriptionGetDimensions(format);
            if (dims.width == (int32_t)width && dims.height == (int32_t)height && CMFormatDescriptionGetMediaSubType(format) == kCVPixelFormatType_32BGRA) selected = format;
        }
        *error = selected ? set(camera->stream, kCMIOStreamPropertyFormatDescription, sizeof(selected), &selected) : -3;
        CFRelease(formats);
    }
    Float64 rate = fps;
    if (!*error) *stage = 4;
    if (!*error) *error = set(camera->stream, kCMIOStreamPropertyFrameRate, sizeof(rate), &rate);
    if (!*error) { *stage = 5; *error = CMIOStreamCopyBufferQueue(camera->stream, altered, NULL, &camera->queue); }
    if (!*error) { *stage = 6; *error = CMIODeviceStartStream(camera->device, camera->stream); }
    if (*error) {
        if (camera->queue) CFRelease(camera->queue);
        free(camera); return NULL;
    }
    return camera;
}

// Shared by real transport and the non-installing IOSurface probe. Checking
// the actual sample's tags here also catches changes to pool attachments.
static CMSampleBufferRef sample(CVPixelBufferRef pixels, int64_t pts, uint32_t fps) {
    if (!CVPixelBufferGetIOSurface(pixels) || CVPixelBufferGetPixelFormatType(pixels) != kCVPixelFormatType_32BGRA) return NULL;
    CVBufferSetAttachment(pixels, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, kCVAttachmentMode_ShouldPropagate);
    CVBufferSetAttachment(pixels, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2, kCVAttachmentMode_ShouldPropagate);
    CVBufferSetAttachment(pixels, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVAttachmentMode_ShouldPropagate);
    CMVideoFormatDescriptionRef format = NULL;
    if (CMVideoFormatDescriptionCreateForImageBuffer(NULL, pixels, &format)) return NULL;
    CMSampleTimingInfo timing = { CMTimeMake(1, fps), CMTimeMake(pts, 1000000000), kCMTimeInvalid };
    CMSampleBufferRef buffer = NULL;
    OSStatus result = CMSampleBufferCreateReadyWithImageBuffer(NULL, pixels, format, &timing, &buffer);
    CFRelease(format);
    return result ? NULL : buffer;
}

int32_t isocast_camera_send(void *handle, CVPixelBufferRef pixels, int64_t pts) {
    IsocastCamera *camera = handle;
    if (CVPixelBufferGetWidth(pixels) != camera->width || CVPixelBufferGetHeight(pixels) != camera->height) return -5;
    if (CMSimpleQueueGetCount(camera->queue) >= CMSimpleQueueGetCapacity(camera->queue)) return 1;
    CMSampleBufferRef buffer = sample(pixels, pts, camera->fps);
    if (!buffer) return -6;
    CMIOSampleBufferSetSequenceNumber(NULL, buffer, ++camera->sequence);
    CMIOSampleBufferSetDiscontinuityFlags(NULL, buffer, 0);
    OSStatus status = CMSimpleQueueEnqueue(camera->queue, buffer);
    // On success ownership goes to CMIO (CMSimpleQueue itself doesn't retain).
    if (status) CFRelease(buffer);
    return status;
}

void isocast_camera_close(void *handle) {
    IsocastCamera *camera = handle;
    CMIODeviceStopStream(camera->device, camera->stream);
    const void *buffer;
    while ((buffer = CMSimpleQueueDequeue(camera->queue))) CFRelease(buffer);
    CFRelease(camera->queue);
    free(camera);
}

int32_t isocast_camera_validate(CVPixelBufferRef pixels, int64_t pts, uint32_t fps) {
    CMSampleBufferRef buffer = sample(pixels, pts, fps);
    if (!buffer) return -1;
    CMFormatDescriptionRef format = CMSampleBufferGetFormatDescription(buffer);
    CFTypeRef primaries = CMFormatDescriptionGetExtension(format, kCMFormatDescriptionExtension_ColorPrimaries);
    CFTypeRef transfer = CMFormatDescriptionGetExtension(format, kCMFormatDescriptionExtension_TransferFunction);
    CFTypeRef matrix = CMFormatDescriptionGetExtension(format, kCMFormatDescriptionExtension_YCbCrMatrix);
    bool valid = primaries && transfer && matrix &&
        CFEqual(primaries, kCVImageBufferColorPrimaries_ITU_R_709_2) &&
        CFEqual(transfer, kCVImageBufferTransferFunction_ITU_R_709_2) &&
        CFEqual(matrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2) &&
        CMTimeCompare(CMSampleBufferGetPresentationTimeStamp(buffer), CMTimeMake(pts, 1000000000)) == 0 &&
        CMTimeCompare(CMSampleBufferGetDuration(buffer), CMTimeMake(1, fps)) == 0;
    CFRelease(buffer);
    return valid ? 0 : -2;
}

static int32_t probe_result(NSDictionary *values, int32_t status) {
    NSData *json = [NSJSONSerialization dataWithJSONObject:values options:NSJSONWritingSortedKeys error:nil];
    if (json) { fwrite(json.bytes, 1, json.length, stdout); fputc('\n', stdout); }
    return status;
}

@interface IsocastCameraActivation : NSObject <OSSystemExtensionRequestDelegate>
@property BOOL done;
@property int result;
@property BOOL activating;
@property BOOL waitForApproval;
@end
@implementation IsocastCameraActivation
- (void)requestNeedsUserApproval:(OSSystemExtensionRequest *)request {
    (void)request;
    fprintf(stderr, "%s\n", self.activating
        ? "Approve Isocast Virtual Camera in System Settings > General > Login Items & Extensions > Camera Extensions."
        : "Approve macOS's request to remove Isocast Virtual Camera.");
    if (self.waitForApproval) {
        // The engine's Start: tell the parent at once, then keep the request
        // alive so sysextd reports the result when the user allows it.
        fputs("needs-approval\n", stdout); fflush(stdout);
        return;
    }
    self.result = 2; self.done = YES; // sysextd continues after approval; don't keep a CLI waiting.
}
- (OSSystemExtensionReplacementAction)request:(OSSystemExtensionRequest *)request actionForReplacingExtension:(OSSystemExtensionProperties *)existing withExtension:(OSSystemExtensionProperties *)extension {
    (void)request; (void)existing; (void)extension; return OSSystemExtensionReplacementActionReplace;
}
- (void)request:(OSSystemExtensionRequest *)request didFinishWithResult:(OSSystemExtensionRequestResult)result {
    (void)request;
    if (result == OSSystemExtensionRequestCompleted) {
        fprintf(stderr, "Isocast camera %s.\n", self.activating ? "activated" : "deactivated");
        self.result = 0;
    } else {
        fprintf(stderr, "Restart macOS to finish %s the Isocast camera; keep its host app until then.\n", self.activating ? "activating" : "deactivating");
        self.result = 2;
    }
    self.done = YES;
}
- (void)request:(OSSystemExtensionRequest *)request didFailWithError:(NSError *)error {
    (void)request;
    if (!self.activating && [error.domain isEqualToString:OSSystemExtensionErrorDomain]
        && error.code == OSSystemExtensionErrorExtensionNotFound) {
        fprintf(stderr, "Isocast camera is already absent from this deactivation request.\n");
        self.result = 0; self.done = YES;
        return;
    }
    if ([error.domain isEqualToString:OSSystemExtensionErrorDomain] && error.code == OSSystemExtensionErrorAuthorizationRequired) {
        // Seen from an engine started over ssh: no login session to ask in.
        fprintf(stderr, "Isocast camera %s failed: macOS couldn't ask for an administrator password (run it from the Mac's logged-in session)\n", self.activating ? "activation" : "deactivation");
        self.result = 1; self.done = YES;
        return;
    }
    fprintf(stderr, "Isocast camera %s failed: %s\n", self.activating ? "activation" : "deactivation", error.localizedDescription.UTF8String);
    self.result = 1; self.done = YES;
}
@end

// What sysextd knows about this app's camera extension (#69): every build
// it has activated or is waiting on, from an OSSystemExtensionRequest for
// properties (macOS 12+), which needs this process's main run loop.
@interface IsocastCameraProperties : NSObject <OSSystemExtensionRequestDelegate>
@property BOOL done;
@property NSArray<OSSystemExtensionProperties *> *found;
@property NSError *error;
@end
@implementation IsocastCameraProperties
- (void)requestNeedsUserApproval:(OSSystemExtensionRequest *)request { (void)request; }
- (OSSystemExtensionReplacementAction)request:(OSSystemExtensionRequest *)request actionForReplacingExtension:(OSSystemExtensionProperties *)existing withExtension:(OSSystemExtensionProperties *)extension {
    (void)request; (void)existing; (void)extension; return OSSystemExtensionReplacementActionCancel;
}
- (void)request:(OSSystemExtensionRequest *)request foundProperties:(NSArray<OSSystemExtensionProperties *> *)properties {
    (void)request; self.found = properties; self.done = YES;
}
- (void)request:(OSSystemExtensionRequest *)request didFinishWithResult:(OSSystemExtensionRequestResult)result {
    (void)request; (void)result; self.done = YES;
}
- (void)request:(OSSystemExtensionRequest *)request didFailWithError:(NSError *)error {
    (void)request; self.error = error; self.done = YES;
}
@end

static NSString *embedded_version(void) {
    NSURL *url = [[NSBundle mainBundle].bundleURL URLByAppendingPathComponent:@"Contents/Library/SystemExtensions/dev.isocast.isocastd.camera.systemextension"];
    return [[NSBundle bundleWithURL:url] objectForInfoDictionaryKey:@"CFBundleVersion"];
}

static NSArray<OSSystemExtensionProperties *> *camera_properties(NSError **error) {
    IsocastCameraProperties *delegate = [IsocastCameraProperties new];
    OSSystemExtensionRequest *request = [OSSystemExtensionRequest propertiesRequestForExtension:@"dev.isocast.isocastd.camera" queue:dispatch_get_main_queue()];
    request.delegate = delegate;
    [[OSSystemExtensionManager sharedManager] submitRequest:request];
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:15];
    while (!delegate.done && deadline.timeIntervalSinceNow > 0) [[NSRunLoop mainRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
    if (!delegate.done) {
        *error = [NSError errorWithDomain:@"dev.isocast" code:1 userInfo:@{NSLocalizedDescriptionKey: @"macOS didn't answer in 15 s"}];
        return nil;
    }
    *error = delegate.error;
    return delegate.error ? nil : (delegate.found ?: @[]);
}

// The activated build that isn't this app's, if one is enabled and none of
// this app's build is: an update replaced the app but not the extension.
static NSString *outdated_version(NSArray<OSSystemExtensionProperties *> *found, NSString *embedded) {
    NSString *other = nil;
    for (OSSystemExtensionProperties *p in found) {
        if (!p.isEnabled || p.isUninstalling) continue;
        if ([p.bundleVersion isEqualToString:embedded]) return nil;
        other = p.bundleVersion;
    }
    return other;
}

// `isocastd --camera-extension-state`: one JSON line. state is current
// (this build is enabled), outdated (another build is), waiting (macOS
// waits for the user's approval) or absent.
int32_t isocast_camera_state(void) {
    @autoreleasepool {
        NSString *embedded = embedded_version();
        if (!embedded) return probe_result(@{@"error": @"this engine doesn't carry the camera extension"}, 1);
        NSError *error = nil;
        NSArray<OSSystemExtensionProperties *> *found = camera_properties(&error);
        if (!found) return probe_result(@{@"error": error.localizedDescription ?: @"properties request failed"}, 1);
        NSMutableArray *builds = [NSMutableArray array];
        NSString *state = @"absent";
        BOOL current = NO, waiting = NO, enabled = NO;
        for (OSSystemExtensionProperties *p in found) {
            [builds addObject:@{@"version": p.bundleVersion ?: @"", @"short_version": p.bundleShortVersion ?: @"",
                @"enabled": @(p.isEnabled), @"awaiting_approval": @(p.isAwaitingUserApproval), @"uninstalling": @(p.isUninstalling)}];
            if (p.isUninstalling) continue;
            if (p.isEnabled) { enabled = YES; if ([p.bundleVersion isEqualToString:embedded]) current = YES; }
            if (p.isAwaitingUserApproval) waiting = YES;
        }
        if (current) state = @"current";
        else if (enabled) state = @"outdated";
        else if (waiting) state = @"waiting";
        return probe_result(@{@"state": state, @"embedded_version": embedded, @"builds": builds}, 0);
    }
}

// wait: keep waiting (up to 15 minutes) after macOS asks the user to approve,
// printing "needs-approval" on stdout first; otherwise return 2 at once.
static int32_t camera_request(bool activate, bool wait) {
    IsocastCameraActivation *delegate = [IsocastCameraActivation new];
    delegate.activating = activate;
    delegate.waitForApproval = wait;
    OSSystemExtensionRequest *request = activate
        ? [OSSystemExtensionRequest activationRequestForExtension:@"dev.isocast.isocastd.camera" queue:dispatch_get_main_queue()]
        : [OSSystemExtensionRequest deactivationRequestForExtension:@"dev.isocast.isocastd.camera" queue:dispatch_get_main_queue()];
    request.delegate = delegate;
    [[OSSystemExtensionManager sharedManager] submitRequest:request];
    // Deactivation asks for an administrator password first (macOS 26.7):
    // give the person time to type it before giving up.
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:wait ? 900 : activate ? 30 : 180];
    while (!delegate.done && deadline.timeIntervalSinceNow > 0) [[NSRunLoop mainRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.1]];
    if (!delegate.done) { fprintf(stderr, "Isocast camera request timed out; check System Settings and retry --check.\n"); return 2; }
    return delegate.result;
}

int32_t isocast_camera_manage(bool activate, bool wait) {
    @autoreleasepool {
        if (![[NSBundle mainBundle].bundlePath hasPrefix:@"/Applications/"]) {
            fprintf(stderr, "Install the signed Isocast host app in /Applications first; see docs/virtual-camera.md.\n");
            return 1;
        }
        if (!activate) {
            // macOS only deactivates the build this app carries ("not
            // found" otherwise): after an update, replace the active older
            // build first (no approval needed), then remove it (#69).
            NSString *embedded = embedded_version();
            NSError *error = nil;
            NSArray<OSSystemExtensionProperties *> *found = embedded ? camera_properties(&error) : nil;
            NSString *outdated = found ? outdated_version(found, embedded) : nil;
            BOOL present = NO;
            for (OSSystemExtensionProperties *p in found) {
                if (!p.isUninstalling && (p.isEnabled || p.isAwaitingUserApproval)) present = YES;
            }
            if (found && !present) {
                // Nothing to remove: don't ask for a password for nothing.
                fprintf(stderr, "Isocast camera is already absent.\n");
                return 0;
            }
            if (outdated) {
                fprintf(stderr, "Isocast camera build %s is active; replacing it with this app's build %s before removing it.\n", outdated.UTF8String, embedded.UTF8String);
                int32_t replaced = camera_request(true, false);
                if (replaced) {
                    fprintf(stderr, "Isocast camera deactivation failed: couldn't replace the active build %s with this app's (%d)\n", outdated.UTF8String, replaced);
                    return 1;
                }
            }
        }
        return camera_request(activate, wait);
    }
}

// Installation acceptance uses AVFoundation, not the diagnostic Mach reader.
// This probe only opens our fixed device UID; no other camera is ever opened.
@interface IsocastCameraProbe : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate>
@property NSUInteger frames;
@property NSUInteger distinct;
@property uint64_t lastHash;
@property CMTime first;
@property CMTime last;
@property BOOL monotonic;
@property size_t width;
@property size_t height;
@end

@implementation IsocastCameraProbe
- (void)captureOutput:(AVCaptureOutput *)output didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer fromConnection:(AVCaptureConnection *)connection {
    (void)output; (void)connection;
    @autoreleasepool {
        CVPixelBufferRef pixels = CMSampleBufferGetImageBuffer(sampleBuffer);
        if (!pixels || CVPixelBufferGetPixelFormatType(pixels) != kCVPixelFormatType_32BGRA || CVPixelBufferLockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly)) return;
        const uint8_t *base = CVPixelBufferGetBaseAddress(pixels);
        size_t width = CVPixelBufferGetWidth(pixels), height = CVPixelBufferGetHeight(pixels), row = CVPixelBufferGetBytesPerRow(pixels);
        uint64_t hash = 1469598103934665603ULL;
        if (base) {
            for (size_t y = 0; y < height; y += 31) {
                for (size_t x = 0; x < width; x += 31) {
                    uint32_t value;
                    memcpy(&value, base + y * row + x * 4, sizeof(value));
                    hash = (hash ^ value) * 1099511628211ULL;
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
        CMTime pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
        @synchronized(self) {
            if (!self.frames) { self.first = pts; self.monotonic = CMTIME_IS_NUMERIC(pts); }
            else if (!CMTIME_IS_NUMERIC(pts) || CMTimeCompare(pts, self.last) <= 0) self.monotonic = NO;
            if (!self.frames || hash != self.lastHash) self.distinct++;
            self.frames++; self.lastHash = hash; self.last = pts;
            self.width = width; self.height = height;
        }
    }
}
@end

int32_t isocast_camera_probe(bool capture) {
    @autoreleasepool {
        AVCaptureDeviceDiscoverySession *discovery = [AVCaptureDeviceDiscoverySession
            discoverySessionWithDeviceTypes:@[AVCaptureDeviceTypeExternal]
            mediaType:AVMediaTypeVideo position:AVCaptureDevicePositionUnspecified];
        AVCaptureDevice *camera = nil;
        for (AVCaptureDevice *device in discovery.devices) {
            if ([device.uniqueID isEqualToString:(__bridge NSString *)CAMERA_UID]) camera = device;
        }
        if (!camera) return probe_result(@{@"available": @NO, @"error": @"Isocast Virtual Camera is not listed by AVFoundation; finish its System Settings approval and retry scripts/vcam-install.sh --check"}, 1);
        if (!capture) return probe_result(@{@"available": @YES, @"id": camera.uniqueID, @"name": camera.localizedName}, 0);
        if ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo] == AVAuthorizationStatusNotDetermined) {
            dispatch_semaphore_t decided = dispatch_semaphore_create(0);
            [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo completionHandler:^(BOOL granted) {
                (void)granted; dispatch_semaphore_signal(decided);
            }];
            NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:60];
            while (dispatch_semaphore_wait(decided, DISPATCH_TIME_NOW) != 0 && deadline.timeIntervalSinceNow > 0)
                [[NSRunLoop mainRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.1]];
        }
        if ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo] != AVAuthorizationStatusAuthorized)
            return probe_result(@{@"available": @YES, @"error": @"Allow camera access for Isocast Camera Host in System Settings > Privacy & Security > Camera, then retry --check"}, 1);
        NSError *error = nil;
        AVCaptureDeviceInput *input = [AVCaptureDeviceInput deviceInputWithDevice:camera error:&error];
        if (!input) return probe_result(@{@"error": error.localizedDescription ?: @"Cannot open Isocast camera"}, 1);
        AVCaptureSession *session = [AVCaptureSession new];
        AVCaptureVideoDataOutput *output = [AVCaptureVideoDataOutput new];
        output.videoSettings = @{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA)};
        output.alwaysDiscardsLateVideoFrames = YES;
        IsocastCameraProbe *probe = [IsocastCameraProbe new];
        dispatch_queue_t queue = dispatch_queue_create("dev.isocast.camera.install-check", DISPATCH_QUEUE_SERIAL);
        [output setSampleBufferDelegate:probe queue:queue];
        if (![session canAddInput:input] || ![session canAddOutput:output])
            return probe_result(@{@"error": @"AVFoundation cannot connect the Isocast camera probe"}, 1);
        [session beginConfiguration];
        [session addInput:input]; [session addOutput:output];
        if ([session canSetSessionPreset:AVCaptureSessionPreset1920x1080])
            session.sessionPreset = AVCaptureSessionPreset1920x1080;
        [session commitConfiguration];
        [session startRunning];
        NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:4];
        while (deadline.timeIntervalSinceNow > 0)
            [[NSRunLoop mainRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
        [session stopRunning];
        [output setSampleBufferDelegate:nil queue:NULL];
        dispatch_sync(queue, ^{});
        @synchronized(probe) {
            double duration = CMTimeGetSeconds(CMTimeSubtract(probe.last, probe.first));
            BOOL valid = probe.frames >= 30 && probe.distinct >= 2 && probe.monotonic && probe.width == 1920 && probe.height == 1080;
            return probe_result(@{@"available": @YES, @"id": camera.uniqueID, @"frames": @(probe.frames),
                @"distinct_frames": @(probe.distinct), @"monotonic_pts": @(probe.monotonic),
                @"width": @(probe.width), @"height": @(probe.height),
                @"fps": @(duration > 0 ? (probe.frames - 1) / duration : 0), @"passed": @(valid)}, valid ? 0 : 1);
        }
    }
}
