// Reads the system-wide cursor (whatever app is under the pointer) so it can
// be mirrored to the RDP client as a pointer shape.
#import <AppKit/AppKit.h>
#include <math.h>
#include <stdint.h>
#include <string.h>

// Private CoreGraphics call; increments whenever the cursor changes.
extern int CGSCurrentCursorSeed(void);

int macrdp_cursor_seed(void) {
    return CGSCurrentCursorSeed();
}

// Renders the current cursor at `scale` pixels per point, capped at
// `max_side` pixels per side. Writes premultiplied BGRA, top row first,
// into `out` (`out_cap` bytes). Returns 1 on success, 0 if no cursor.
int macrdp_cursor_snapshot(double scale, int max_side, uint8_t *out, size_t out_cap,
                           int *out_w, int *out_h, int *out_hot_x, int *out_hot_y) {
    @autoreleasepool {
        NSCursor *cursor = [NSCursor currentSystemCursor];
        NSImage *image = cursor.image;
        if (cursor == nil || image == nil) return 0;

        NSSize size = image.size;
        if (size.width < 1 || size.height < 1) return 0;
        double longest = fmax(size.width, size.height) * scale;
        if (longest > max_side) scale *= max_side / longest;

        int w = (int)ceil(size.width * scale);
        int h = (int)ceil(size.height * scale);
        if (w < 1 || h < 1 || (size_t)w * h * 4 > out_cap) return 0;
        memset(out, 0, (size_t)w * h * 4);

        CGColorSpaceRef space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        CGContextRef ctx = CGBitmapContextCreate(out, w, h, 8, (size_t)w * 4, space,
            kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
        CGColorSpaceRelease(space);
        if (ctx == NULL) return 0;

        NSGraphicsContext *gc = [NSGraphicsContext graphicsContextWithCGContext:ctx flipped:NO];
        [NSGraphicsContext saveGraphicsState];
        [NSGraphicsContext setCurrentContext:gc];
        [image drawInRect:NSMakeRect(0, 0, w, h)
                 fromRect:NSZeroRect
                operation:NSCompositingOperationCopy
                 fraction:1.0];
        [NSGraphicsContext restoreGraphicsState];
        CGContextFlush(ctx);
        CGContextRelease(ctx);

        NSPoint hot = cursor.hotSpot; // points, from the top-left
        *out_w = w;
        *out_h = h;
        *out_hot_x = (int)fmin(fmax(round(hot.x * scale), 0), w - 1);
        *out_hot_y = (int)fmin(fmax(round(hot.y * scale), 0), h - 1);
        return 1;
    }
}
