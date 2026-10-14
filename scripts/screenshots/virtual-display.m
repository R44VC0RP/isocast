// A temporary HiDPI display for the screenshots, for test Macs whose own
// screen is too small for the app window (isocast-m2-b: 960x720 points at
// 2x). It lasts as long as this process:
//
//   clang -fobjc-arc -framework Foundation -framework CoreGraphics \
//       virtual-display.m -o virtual-display
//   ./virtual-display 1440 960 &     # points; pixels are twice that
//
// Prints "display ID X Y W H" (its id and bounds in points) once it is up.
// Uses CoreGraphics' CGVirtualDisplay classes, which macOS has but doesn't
// declare in a public header.
#import <CoreGraphics/CoreGraphics.h>
#import <Foundation/Foundation.h>

@interface CGVirtualDisplayDescriptor : NSObject
@property(retain, nonatomic) dispatch_queue_t queue;
@property(retain, nonatomic) NSString *name;
@property(nonatomic) unsigned int maxPixelsHigh;
@property(nonatomic) unsigned int maxPixelsWide;
@property(nonatomic) CGSize sizeInMillimeters;
@property(nonatomic) unsigned int productID;
@property(nonatomic) unsigned int vendorID;
@property(nonatomic) unsigned int serialNum;
@end

@interface CGVirtualDisplayMode : NSObject
- (instancetype)initWithWidth:(unsigned int)width
                       height:(unsigned int)height
                  refreshRate:(double)refreshRate;
@end

@interface CGVirtualDisplaySettings : NSObject
@property(retain, nonatomic) NSArray *modes;
@property(nonatomic) unsigned int hiDPI;
@end

@interface CGVirtualDisplay : NSObject
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor *)descriptor;
- (BOOL)applySettings:(CGVirtualDisplaySettings *)settings;
@property(readonly, nonatomic) unsigned int displayID;
@end

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        unsigned int width = argc > 1 ? (unsigned int)atoi(argv[1]) : 1440;
        unsigned int height = argc > 2 ? (unsigned int)atoi(argv[2]) : 960;

        CGVirtualDisplayDescriptor *descriptor = [[CGVirtualDisplayDescriptor alloc] init];
        descriptor.queue = dispatch_get_main_queue();
        descriptor.name = @"Isocast Screenshots";
        descriptor.maxPixelsWide = width * 2;
        descriptor.maxPixelsHigh = height * 2;
        descriptor.sizeInMillimeters = CGSizeMake(width * 0.2, height * 0.2);
        descriptor.productID = 0x1234;
        descriptor.vendorID = 0x3456;
        descriptor.serialNum = 1;

        CGVirtualDisplay *display = [[CGVirtualDisplay alloc] initWithDescriptor:descriptor];
        if (display == nil) {
            fprintf(stderr, "virtual-display: couldn't create the display\n");
            return 1;
        }
        CGVirtualDisplaySettings *settings = [[CGVirtualDisplaySettings alloc] init];
        settings.hiDPI = 1;
        settings.modes = @[ [[CGVirtualDisplayMode alloc] initWithWidth:width
                                                                height:height
                                                           refreshRate:60] ];
        if (![display applySettings:settings]) {
            fprintf(stderr, "virtual-display: couldn't apply its mode\n");
            return 1;
        }
        // Let WindowServer arrange it, then say where it is.
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
            CGRect bounds = CGDisplayBounds(display.displayID);
            CGDisplayModeRef mode = CGDisplayCopyDisplayMode(display.displayID);
            printf("display %u %g %g %g %g (%zux%zu pixels)\n", display.displayID, bounds.origin.x,
                   bounds.origin.y, bounds.size.width, bounds.size.height,
                   mode ? CGDisplayModeGetPixelWidth(mode) : 0, mode ? CGDisplayModeGetPixelHeight(mode) : 0);
            fflush(stdout);
            if (mode) CGDisplayModeRelease(mode);
        });
        dispatch_main();
    }
    return 0;
}
