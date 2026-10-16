// A virtual display sized to the RDP client, so the Mac's desktop matches the
// client window 1:1. Uses CoreGraphics' private CGVirtualDisplay classes (the
// ones Screen Sharing and display utilities use); everything is looked up at
// runtime so a missing class fails cleanly instead of crashing.
#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <objc/message.h>
#import <objc/runtime.h>
#include <stdint.h>
#include <unistd.h>

#define MAX_MIRRORED 16

typedef struct {
    void *display;                       // retained CGVirtualDisplay
    uint32_t display_id;
    uint32_t mirrored[MAX_MIRRORED];     // displays we made mirror it
    uint32_t mirrored_count;
} macrdp_vd;

static BOOL display_online(CGDirectDisplayID target) {
    CGDirectDisplayID ids[32];
    uint32_t n = 0;
    if (CGGetOnlineDisplayList(32, ids, &n) != kCGErrorSuccess) return NO;
    for (uint32_t i = 0; i < n; i++) if (ids[i] == target) return YES;
    return NO;
}

static BOOL display_has_size(CGDirectDisplayID target, size_t w, size_t h) {
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(target);
    if (mode == NULL) return NO;
    BOOL ok = CGDisplayModeGetPixelWidth(mode) == w && CGDisplayModeGetPixelHeight(mode) == h;
    CGDisplayModeRelease(mode);
    return ok;
}

// Creates the display, waits until it's online at w x h, makes it the main
// display and mirrors every other display onto it. Returns NULL on failure;
// `error` gets a short reason.
macrdp_vd *macrdp_vd_create(uint32_t w, uint32_t h, char *error, size_t error_len) {
    @autoreleasepool {
        #define FAIL(msg) do { snprintf(error, error_len, "%s", msg); return NULL; } while (0)
        Class Descriptor = objc_getClass("CGVirtualDisplayDescriptor");
        Class Display = objc_getClass("CGVirtualDisplay");
        Class Settings = objc_getClass("CGVirtualDisplaySettings");
        Class Mode = objc_getClass("CGVirtualDisplayMode");
        if (!Descriptor || !Display || !Settings || !Mode) FAIL("CGVirtualDisplay API not available");

        id desc = [[Descriptor alloc] init];
        [desc setValue:@"macrdp" forKey:@"name"];
        [desc setValue:@(w) forKey:@"maxPixelsWide"];
        [desc setValue:@(h) forKey:@"maxPixelsHigh"];
        // ~96 dpi, so macOS treats it as a normal (non-Retina) monitor
        CGSize mm = CGSizeMake(w * 25.4 / 96.0, h * 25.4 / 96.0);
        [desc setValue:[NSValue valueWithBytes:&mm objCType:@encode(CGSize)] forKey:@"sizeInMillimeters"];
        [desc setValue:@(0x6d72) forKey:@"vendorID"];   // "mr"
        [desc setValue:@(0x6470) forKey:@"productID"];  // "dp"
        // macOS remembers settings per vendor/product/serial; one serial per size
        // keeps each size's arrangement separate
        [desc setValue:@((w << 16) ^ h) forKey:@"serialNum"];
        // No main run loop in this process: give it its own queue
        [desc setValue:dispatch_queue_create("macrdp.virtual-display", DISPATCH_QUEUE_SERIAL) forKey:@"queue"];

        id display = ((id (*)(id, SEL, id))objc_msgSend)([Display alloc], sel_registerName("initWithDescriptor:"), desc);
        if (display == nil) FAIL("CGVirtualDisplay initWithDescriptor failed");

        id mode = ((id (*)(id, SEL, NSUInteger, NSUInteger, double))objc_msgSend)(
            [Mode alloc], sel_registerName("initWithWidth:height:refreshRate:"), (NSUInteger)w, (NSUInteger)h, 60.0);
        id settings = [[Settings alloc] init];
        [settings setValue:@NO forKey:@"hiDPI"];
        [settings setValue:@[ mode ] forKey:@"modes"];
        BOOL applied = ((BOOL (*)(id, SEL, id))objc_msgSend)(display, sel_registerName("applySettings:"), settings);
        if (!applied) FAIL("CGVirtualDisplay applySettings failed");

        CGDirectDisplayID vid = (CGDirectDisplayID)[[display valueForKey:@"displayID"] unsignedIntValue];
        // Normally ready at once; if not, keep re-applying the mode for up to 10 s
        for (int i = 0; i < 200 && !(display_online(vid) && display_has_size(vid, w, h)); i++) {
            if (i > 0 && i % 20 == 0) ((BOOL (*)(id, SEL, id))objc_msgSend)(display, sel_registerName("applySettings:"), settings);
            usleep(50000);
        }
        if (!display_online(vid)) FAIL("virtual display did not come online");
        if (!display_has_size(vid, w, h)) {
            CGDisplayModeRef m = CGDisplayCopyDisplayMode(vid);
            snprintf(error, error_len, "virtual display %u came up at %zux%zu", vid,
                     m ? CGDisplayModeGetPixelWidth(m) : 0, m ? CGDisplayModeGetPixelHeight(m) : 0);
            if (m) CGDisplayModeRelease(m);
            return NULL;
        }

        macrdp_vd *vd = calloc(1, sizeof(macrdp_vd));
        vd->display = (__bridge_retained void *)display;
        vd->display_id = vid;

        // One session-only change: virtual display at (0,0) (= main), others mirror it
        CGDisplayConfigRef config;
        if (CGBeginDisplayConfiguration(&config) == kCGErrorSuccess) {
            CGConfigureDisplayOrigin(config, vid, 0, 0);
            CGDirectDisplayID ids[32];
            uint32_t n = 0;
            CGGetOnlineDisplayList(32, ids, &n);
            for (uint32_t i = 0; i < n && vd->mirrored_count < MAX_MIRRORED; i++) {
                if (ids[i] == vid) continue;
                if (CGConfigureDisplayMirrorOfDisplay(config, ids[i], vid) == kCGErrorSuccess) {
                    vd->mirrored[vd->mirrored_count++] = ids[i];
                }
            }
            if (CGCompleteDisplayConfiguration(config, kCGConfigureForSession) != kCGErrorSuccess) {
                vd->mirrored_count = 0;
            }
        }
        // Wait for it to become main (the mirror change is asynchronous)
        for (int i = 0; i < 40 && CGMainDisplayID() != vid; i++) usleep(50000);
        return vd;
        #undef FAIL
    }
}

uint32_t macrdp_vd_display_id(const macrdp_vd *vd) {
    return vd->display_id;
}

// Undoes the mirroring and removes the display.
void macrdp_vd_destroy(macrdp_vd *vd) {
    if (vd == NULL) return;
    @autoreleasepool {
        if (vd->mirrored_count > 0) {
            CGDisplayConfigRef config;
            if (CGBeginDisplayConfiguration(&config) == kCGErrorSuccess) {
                for (uint32_t i = 0; i < vd->mirrored_count; i++) {
                    CGConfigureDisplayMirrorOfDisplay(config, vd->mirrored[i], kCGNullDirectDisplay);
                }
                CGCompleteDisplayConfiguration(config, kCGConfigureForSession);
            }
        }
        // Dropping the last reference removes the display
        (void)CFBridgingRelease(vd->display);
        CGDirectDisplayID vid = vd->display_id;
        free(vd);
        // Wait until macOS has settled: our display gone and another one main
        for (int i = 0; i < 100 && (display_online(vid) || CGMainDisplayID() == vid); i++) usleep(50000);
    }
}
