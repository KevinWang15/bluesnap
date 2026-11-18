// SCScreenshotManager captures the same shadow-free edge coverage as the
// system Screenshot tool. Load it on demand so the binary still runs on 11.
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>

CGImageRef bluesnap_copy_window_image(CGWindowID windowID, CGSize pointSize, size_t width, size_t height) {
    if (@available(macOS 14.0, *)) {
        NSBundle *kit = [NSBundle bundleWithPath:@"/System/Library/Frameworks/ScreenCaptureKit.framework"];
        Class shareable = [kit classNamed:@"SCShareableContent"];
        Class manager = [kit classNamed:@"SCScreenshotManager"];
        if (!shareable || !manager) return NULL;

        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        // ARC keeps the returned CGImage and the block's storage alive even
        // if the service completes after the caller has timed out.
        __block id result = nil;
        [shareable getShareableContentExcludingDesktopWindows:YES onScreenWindowsOnly:YES
            completionHandler:^(SCShareableContent *content, NSError * __unused error) {
                for (SCWindow *window in content.windows) {
                    if (window.windowID != windowID || !CGSizeEqualToSize(window.frame.size, pointSize)) continue;
                    SCContentFilter *filter = [[[kit classNamed:@"SCContentFilter"] alloc] initWithDesktopIndependentWindow:window];
                    SCStreamConfiguration *config = [[kit classNamed:@"SCStreamConfiguration"] new];
                    config.showsCursor = NO;
                    config.ignoreShadowsSingleWindow = YES;
                    config.shouldBeOpaque = NO;
                    config.width = width;
                    config.height = height;
                    [manager captureImageWithFilter:filter configuration:config completionHandler:^(CGImageRef image, NSError * __unused error) {
                        result = (__bridge id)image;
                        dispatch_semaphore_signal(done);
                    }];
                    return;
                }
                dispatch_semaphore_signal(done);
            }];
        // Window picking runs on the event thread. Fall back to Quartz if the
        // service stalls (for example while screen capture access is changing).
        if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 250 * NSEC_PER_MSEC)) == 0) {
            return (__bridge_retained CGImageRef)result;
        }
    }
    return NULL;
}
