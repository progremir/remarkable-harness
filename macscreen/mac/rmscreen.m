// rmscreen: mirror a single Mac window to a reMarkable over VNC.
//
// The tablet first shows a list of open windows (drawn here). Tapping one
// mirrors just that window, scaled to fit the portrait panel under a bar that
// leads back to the list. The window stays where it is on the Mac: nothing is
// moved or resized. Touch on the tablet becomes input on that window: a tap
// clicks, a drag scrolls, a long press right-clicks, and the Mac pointer is
// put back where it was afterwards.
//
// Served with a minimal RFB 3.8 server (Raw encoding). The reMarkable viewer
// (VNSee) has no password support, so instead of a password the server accepts
// exactly the tablet's own addresses: its USB link address and its Tailscale
// address (bound to the device's WireGuard key).
//
// Build: ./build.sh (signs with the local certificate so permissions stick)
// Run:   rmscreen <tablet-tailscale-ip>
// Needs Screen Recording (capture) and Accessibility (input) permissions.

#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreText/CoreText.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <ApplicationServices/ApplicationServices.h>
#import <ImageIO/ImageIO.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <pthread.h>
#include <signal.h>
#include <sys/socket.h>
#include <unistd.h>
#include <zlib.h>

// ---- Configuration ----

static const int kPixW = 1620, kPixH = 2160;  // reMarkable Paper Pro panel, portrait
// Current logical size: portrait (1620x2160) or landscape (2160x1620), as the
// viewer reports the tablet's orientation. Same pixel count either way.
static int gW = kPixW, gH = kPixH;
static const int kPort = 5900;
static const int kTile = 32;                  // dirty-region granularity (pixels)
static const int kBarH = 96;                  // top bar in window mode (pixels)
static const int kHeaderH = 240, kRowH = 130; // picker layout (pixels)
static const int kDragThreshold = 24;         // pixels of movement before a touch becomes a drag
static const double kLongPress = 0.6;         // seconds held still: right-click (or pin, in the list)

// ---- Shared state (guarded by gLock) ----

static pthread_mutex_t gLock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t gFrameCond = PTHREAD_COND_INITIALIZER;
static uint8_t *gFrame;        // what the tablet sees, BGRA, kPixW*kPixH*4
static uint64_t gFrameSeq;     // bumps on every change to gFrame

enum { kModePicker, kModeWindow };
static int gMode = kModePicker;
// The app list ("picker"). Each entry: name, bid (bundle id), url, pid
// (NSNumber, running apps only), pinned (NSNumber bool).
static NSArray<NSDictionary *> *gEntries;
static NSString *gQuery = @"";          // search text typed on the tablet keyboard
static CGFloat gScroll;                 // list scroll offset (pixels)
static NSString *gStatus;               // e.g. "Opening Safari…", shown in the header
static SCWindow *gWindow;               // window being mirrored
static CGRect gWinFrame;                // its current frame, global points (top-left origin)
static CGRect gContent;                 // where it is drawn on the tablet (pixels)
static in_addr_t gTabletTailscaleIP;
static uint64_t gStatFrames, gStatBytes, gStatUpdates;  // diagnostics
static uint64_t gResizeSeq;    // bumps when the logical size changes

static void die(NSString *msg) {
  fprintf(stderr, "rmscreen: %s\n", msg.UTF8String);
  exit(1);
}

static void frameChangedLocked(void) {
  gFrameSeq++;
  pthread_cond_broadcast(&gFrameCond);
}

// ---- Drawing into gFrame ----

static CGContextRef frameContext(void) {
  CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
  CGContextRef ctx = CGBitmapContextCreate(gFrame, gW, gH, 8, gW * 4, cs,
                                           kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
  CGColorSpaceRelease(cs);
  return ctx;
}

// Draws one line of text with its baseline `baseline` pixels from the top,
// cut to `maxW` pixels with an ellipsis.
static void drawText(CGContextRef ctx, NSString *text, CGFloat size, bool bold, CGFloat x, CGFloat baseline,
                     bool center, CGFloat maxW) {
  CTFontRef font = CTFontCreateUIFontForLanguage(bold ? kCTFontUIFontEmphasizedSystem : kCTFontUIFontSystem, size, NULL);
  NSDictionary *attrs = @{(__bridge id)kCTFontAttributeName: (__bridge id)font,
                          (__bridge id)kCTForegroundColorAttributeName: (__bridge id)CGColorGetConstantColor(kCGColorBlack)};
  NSAttributedString *as = [[NSAttributedString alloc] initWithString:text attributes:attrs];
  CTLineRef full = CTLineCreateWithAttributedString((__bridge CFAttributedStringRef)as);
  NSAttributedString *dots = [[NSAttributedString alloc] initWithString:@"…" attributes:attrs];
  CTLineRef ellipsis = CTLineCreateWithAttributedString((__bridge CFAttributedStringRef)dots);
  CTLineRef line = CTLineCreateTruncatedLine(full, maxW, kCTLineTruncationEnd, ellipsis);
  if (!line) line = CFRetain(full);
  if (center) x -= CTLineGetTypographicBounds(line, NULL, NULL, NULL) / 2;
  CGContextSetTextMatrix(ctx, CGAffineTransformIdentity);
  CGContextSetTextPosition(ctx, x, gH - baseline);
  CTLineDraw(line, ctx);
  CFRelease(line);
  CFRelease(ellipsis);
  CFRelease(full);
  CFRelease(font);
}

static void fillTopRect(CGContextRef ctx, CGFloat x, CGFloat top, CGFloat w, CGFloat h, CGFloat gray) {
  CGContextSetGrayFillColor(ctx, gray, 1);
  CGContextFillRect(ctx, CGRectMake(x, gH - top - h, w, h));
}

static NSString *windowLabel(SCWindow *w) {
  NSString *app = w.owningApplication.applicationName ?: @"?";
  NSString *title = w.title;
  return title.length ? [NSString stringWithFormat:@"%@ — %@", app, title] : app;
}

static NSInteger visibleRows(void) { return (gH - kHeaderH) / kRowH + 1; }

static CGFloat maxScroll(void) {
  return MAX(0, (CGFloat)gEntries.count * kRowH - (gH - kHeaderH));
}

static void drawPickerLocked(void) {
  gScroll = MIN(MAX(gScroll, 0), maxScroll());
  CGContextRef ctx = frameContext();
  fillTopRect(ctx, 0, 0, gW, gH, 1);
  NSInteger first = (NSInteger)(gScroll / kRowH);
  CGFloat offset = fmod(gScroll, kRowH);
  for (NSInteger i = first; i < (NSInteger)gEntries.count && i < first + visibleRows(); i++) {
    CGFloat top = kHeaderH + (i - first) * kRowH - offset;
    NSDictionary *e = gEntries[i];
    NSString *mark = [e[@"pinned"] boolValue] ? @"★ " : @"";
    drawText(ctx, [mark stringByAppendingString:e[@"name"]], 50, [e[@"pid"] intValue] > 0, 60,
             top + kRowH / 2 + 17, false, gW - 260);
    if ([e[@"pid"] intValue] > 0) drawText(ctx, @"open", 34, false, gW - 160, top + kRowH / 2 + 12, false, 140);
    fillTopRect(ctx, 60, top + kRowH - 2, gW - 120, 2, 0.75);
  }
  // Header over the list.
  fillTopRect(ctx, 0, 0, gW, kHeaderH, 1);
  drawText(ctx, @"Mac apps", 64, true, 60, 100, false, gW / 2);
  drawText(ctx, @"long-press to pin", 30, false, gW - 330, 100, false, 300);
  NSString *search = gStatus ?: (gQuery.length ? [NSString stringWithFormat:@"Search: %@_", gQuery]
                                               : @"Type to search");
  drawText(ctx, search, 44, false, 60, 185, false, gW - 120);
  fillTopRect(ctx, 0, kHeaderH - 4, gW, 4, 0);
  if (gEntries.count == 0) drawText(ctx, @"No matching apps", 46, false, 60, kHeaderH + 90, false, gW - 120);
  CGContextRelease(ctx);
  frameChangedLocked();
}

static void drawWindowChromeLocked(void) {
  CGContextRef ctx = frameContext();
  fillTopRect(ctx, 0, 0, gW, gH, 1);
  fillTopRect(ctx, 0, kBarH - 3, gW, 3, 0);
  drawText(ctx, @"‹ Apps", 40, true, 30, 64, false, 400);
  drawText(ctx, gWindow.owningApplication.applicationName ?: @"", 40, false, gW / 2.0, 64, true, 800);
  CGContextRelease(ctx);
  frameChangedLocked();
}

// ---- Capture of one window ----

@interface Capture : NSObject <SCStreamOutput, SCStreamDelegate>
@property(strong) SCStream *stream;
@end

static Capture *gCapture;

// Fits a window of `frame` points into the area below the bar.
static CGRect contentRectFor(CGRect frame) {
  CGFloat availW = gW, availH = gH - kBarH;
  CGFloat s = MIN(availW / frame.size.width, availH / frame.size.height);
  CGFloat w = floor(frame.size.width * s), h = floor(frame.size.height * s);
  return CGRectMake(floor((availW - w) / 2), kBarH + floor((availH - h) / 2), w, h);
}

static SCStreamConfiguration *configFor(CGRect content) {
  SCStreamConfiguration *cfg = [SCStreamConfiguration new];
  cfg.width = (size_t)content.size.width;
  cfg.height = (size_t)content.size.height;
  cfg.pixelFormat = kCVPixelFormatType_32BGRA;
  cfg.minimumFrameInterval = CMTimeMake(1, 10);  // e-ink can't use more
  cfg.showsCursor = NO;
  cfg.queueDepth = 3;
  return cfg;
}

@implementation Capture
- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sb ofType:(SCStreamOutputType)type {
  if (type != SCStreamOutputTypeScreen) return;
  CFArrayRef att = CMSampleBufferGetSampleAttachmentsArray(sb, false);
  if (att && CFArrayGetCount(att) > 0) {
    CFDictionaryRef d = CFArrayGetValueAtIndex(att, 0);
    CFNumberRef st = CFDictionaryGetValue(d, (__bridge CFStringRef)SCStreamFrameInfoStatus);
    int status = 0;
    if (st) CFNumberGetValue(st, kCFNumberIntType, &status);
    if (status != SCFrameStatusComplete) return;  // idle frames carry no new pixels
  }
  CVImageBufferRef img = CMSampleBufferGetImageBuffer(sb);
  if (!img) return;
  CVPixelBufferLockBaseAddress(img, kCVPixelBufferLock_ReadOnly);
  size_t w = CVPixelBufferGetWidth(img), h = CVPixelBufferGetHeight(img);
  size_t stride = CVPixelBufferGetBytesPerRow(img);
  const uint8_t *base = CVPixelBufferGetBaseAddress(img);
  pthread_mutex_lock(&gLock);
  if (gMode == kModeWindow && stream == gCapture.stream) {
    size_t x0 = (size_t)gContent.origin.x, y0 = (size_t)gContent.origin.y;
    size_t cw = MIN(w, (size_t)gW - x0), ch = MIN(h, (size_t)gH - y0);
    for (size_t y = 0; y < ch; y++) memcpy(gFrame + ((y0 + y) * gW + x0) * 4, base + y * stride, cw * 4);
    gStatFrames++;
    frameChangedLocked();
  }
  pthread_mutex_unlock(&gLock);
  CVPixelBufferUnlockBaseAddress(img, kCVPixelBufferLock_ReadOnly);
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
  fprintf(stderr, "rmscreen: capture stopped: %s\n", error.localizedDescription.UTF8String);
}
@end

static void stopCapture(void) {
  if (gCapture.stream) [gCapture.stream stopCaptureWithCompletionHandler:nil];
  gCapture.stream = nil;
}


// ---- Accessibility helpers for the mirrored window ----

// The AX element of the app's window whose frame matches `f` (retained), or NULL.
static AXUIElementRef copyAXWindow(pid_t pid, CGRect f) {
  AXUIElementRef app = AXUIElementCreateApplication(pid);
  AXUIElementRef found = NULL;
  CFArrayRef wins = NULL;
  if (AXUIElementCopyAttributeValue(app, kAXWindowsAttribute, (CFTypeRef *)&wins) == kAXErrorSuccess && wins) {
    for (CFIndex i = 0; i < CFArrayGetCount(wins) && !found; i++) {
      AXUIElementRef win = (AXUIElementRef)CFArrayGetValueAtIndex(wins, i);
      CFTypeRef posV = NULL, sizeV = NULL;
      CGPoint pos;
      CGSize size;
      if (AXUIElementCopyAttributeValue(win, kAXPositionAttribute, &posV) == kAXErrorSuccess &&
          AXUIElementCopyAttributeValue(win, kAXSizeAttribute, &sizeV) == kAXErrorSuccess &&
          AXValueGetValue(posV, kAXValueCGPointType, &pos) && AXValueGetValue(sizeV, kAXValueCGSizeType, &size) &&
          fabs(pos.x - f.origin.x) < 2 && fabs(pos.y - f.origin.y) < 2 &&
          fabs(size.width - f.size.width) < 2 && fabs(size.height - f.size.height) < 2) {
        found = (AXUIElementRef)CFRetain(win);
      }
      if (posV) CFRelease(posV);
      if (sizeV) CFRelease(sizeV);
    }
    CFRelease(wins);
  }
  CFRelease(app);
  return found;
}

static void setAXFrame(AXUIElementRef win, CGRect f) {
  AXValueRef posV = AXValueCreate(kAXValueCGPointType, &f.origin);
  AXValueRef sizeV = AXValueCreate(kAXValueCGSizeType, &f.size);
  AXUIElementSetAttributeValue(win, kAXSizeAttribute, sizeV);
  AXUIElementSetAttributeValue(win, kAXPositionAttribute, posV);
  CFRelease(posV);
  CFRelease(sizeV);
}

// The mirrored window is resized to the tablet's proportions while shown, and
// put back as it was afterwards.
static bool gHasOrig;
static pid_t gOrigPid;
static CGRect gOrigFrame;

static void resizeForTablet(SCWindow *w) {
  CGSize target = CGSizeMake(gW / 2.0, (gH - kBarH) / 2.0);  // points, at 2x
  pthread_mutex_lock(&gLock);
  CGRect current = gWindow == w ? gWinFrame : w.frame;
  pthread_mutex_unlock(&gLock);
  AXUIElementRef win = copyAXWindow(w.owningApplication.processID, current);
  if (!win) {
    fprintf(stderr, "rmscreen: could not find the window to resize\n");
    return;
  }
  if (!gHasOrig) {
    gHasOrig = true;
    gOrigPid = w.owningApplication.processID;
    gOrigFrame = w.frame;
  }
  setAXFrame(win, CGRectMake(current.origin.x, current.origin.y, target.width, target.height));
  CFRelease(win);
}

static void restoreOriginal(void) {
  if (!gHasOrig) return;
  gHasOrig = false;
  pthread_mutex_lock(&gLock);
  CGRect now = gWinFrame;
  pthread_mutex_unlock(&gLock);
  AXUIElementRef win = copyAXWindow(gOrigPid, now);
  if (win) {
    setAXFrame(win, gOrigFrame);
    CFRelease(win);
  }
}

static void trackWindow(void);

// ---- Window list and switching (main thread) ----

// Installed apps (scanned now and then) and pins (saved on this Mac).
static NSArray<NSDictionary *> *gInstalled;
static NSMutableArray<NSString *> *gPins;

static NSString *pinsPath(void) {
  return [NSHomeDirectory() stringByAppendingPathComponent:@".config/rmscreen/pins.json"];
}

static void loadPins(void) {
  NSData *d = [NSData dataWithContentsOfFile:pinsPath()];
  NSArray *a = d ? [NSJSONSerialization JSONObjectWithData:d options:0 error:nil] : nil;
  gPins = [a isKindOfClass:NSArray.class] ? [a mutableCopy] : [NSMutableArray array];
}

static void savePins(void) {
  [[NSJSONSerialization dataWithJSONObject:gPins options:0 error:nil] writeToFile:pinsPath() atomically:YES];
}

static void scanInstalled(void) {
  NSMutableArray *apps = [NSMutableArray array];
  NSFileManager *fm = NSFileManager.defaultManager;
  NSArray *dirs = @[@"/Applications", @"/Applications/Utilities", @"/System/Applications",
                    @"/System/Applications/Utilities", [NSHomeDirectory() stringByAppendingPathComponent:@"Applications"]];
  for (NSString *dir in dirs) {
    for (NSString *f in [fm contentsOfDirectoryAtPath:dir error:nil]) {
      if (![f.pathExtension isEqualToString:@"app"]) continue;
      NSURL *url = [NSURL fileURLWithPath:[dir stringByAppendingPathComponent:f]];
      NSString *bid = [NSBundle bundleWithURL:url].bundleIdentifier;
      if (!bid) continue;
      NSString *name = [[fm displayNameAtPath:url.path] stringByDeletingPathExtension];
      [apps addObject:@{@"name": name, @"bid": bid, @"url": url}];
    }
  }
  gInstalled = apps;
}

// Installed and running apps, filtered by the search text: pinned first (in pin
// order), then running, then the rest alphabetically.
static NSArray<NSDictionary *> *buildEntries(NSString *query) {
  NSMutableDictionary<NSString *, NSMutableDictionary *> *byBid = [NSMutableDictionary dictionary];
  for (NSDictionary *a in gInstalled) byBid[a[@"bid"]] = [a mutableCopy];
  for (NSRunningApplication *r in NSWorkspace.sharedWorkspace.runningApplications) {
    if (r.activationPolicy != NSApplicationActivationPolicyRegular || !r.bundleIdentifier) continue;
    if ([r.bundleIdentifier isEqualToString:@"local.rmscreen"]) continue;
    NSMutableDictionary *e = byBid[r.bundleIdentifier];
    if (!e) e = byBid[r.bundleIdentifier] = [@{@"name": r.localizedName ?: r.bundleIdentifier, @"bid": r.bundleIdentifier} mutableCopy];
    if (r.bundleURL && !e[@"url"]) e[@"url"] = r.bundleURL;
    e[@"pid"] = @(r.processIdentifier);
  }
  NSMutableArray *list = [NSMutableArray array];
  for (NSMutableDictionary *e in byBid.allValues) {
    if (query.length && [e[@"name"] rangeOfString:query options:NSCaseInsensitiveSearch | NSDiacriticInsensitiveSearch].location == NSNotFound) continue;
    e[@"pinned"] = @([gPins containsObject:e[@"bid"]]);
    [list addObject:e];
  }
  [list sortUsingComparator:^NSComparisonResult(NSDictionary *a, NSDictionary *b) {
    NSUInteger pa = [gPins indexOfObject:a[@"bid"]], pb = [gPins indexOfObject:b[@"bid"]];
    if (pa != pb) return pa < pb ? NSOrderedAscending : NSOrderedDescending;  // NSNotFound sorts last
    bool ra = [a[@"pid"] intValue] > 0, rb = [b[@"pid"] intValue] > 0;
    if (ra != rb) return ra ? NSOrderedAscending : NSOrderedDescending;
    return [a[@"name"] localizedCaseInsensitiveCompare:b[@"name"]];
  }];
  return list;
}

static void redrawPicker(bool resetScroll) {
  NSArray *entries = buildEntries(gQuery);
  pthread_mutex_lock(&gLock);
  gEntries = entries;
  if (resetScroll) gScroll = 0;
  if (gMode == kModePicker) drawPickerLocked();
  pthread_mutex_unlock(&gLock);
}

static void showPicker(void) {
  pthread_mutex_lock(&gLock);
  bool leaving = gMode == kModeWindow;
  pthread_mutex_unlock(&gLock);
  if (leaving) {
    stopCapture();
    restoreOriginal();
  }
  pthread_mutex_lock(&gLock);
  gWindow = nil;
  gMode = kModePicker;
  pthread_mutex_unlock(&gLock);
  redrawPicker(false);
}

static void setStatus(NSString *status) {
  pthread_mutex_lock(&gLock);
  gStatus = status;
  if (gMode == kModePicker) drawPickerLocked();
  pthread_mutex_unlock(&gLock);
}

// The app's frontmost normal window (on-screen order), or 0.
static CGWindowID frontWindowOf(pid_t pid) {
  CFArrayRef info = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID);
  CGWindowID found = 0;
  for (CFIndex i = 0; info && i < CFArrayGetCount(info) && !found; i++) {
    NSDictionary *w = (__bridge NSDictionary *)CFArrayGetValueAtIndex(info, i);
    if ([w[(id)kCGWindowOwnerPID] intValue] != pid || [w[(id)kCGWindowLayer] intValue] != 0) continue;
    CGRect b;
    if (!CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)w[(id)kCGWindowBounds], &b)) continue;
    if (b.size.width >= 200 && b.size.height >= 150) found = [w[(id)kCGWindowNumber] unsignedIntValue];
  }
  if (info) CFRelease(info);
  return found;
}

static void openWindow(SCWindow *w);

// Mirrors the app's front window; launches or reopens the app if it has none,
// then keeps looking for a window for a few seconds.
static void openApp(NSDictionary *e) {
  NSString *bid = e[@"bid"], *name = e[@"name"];
  NSURL *url = e[@"url"];
  __block int tries = 0;
  __block void (^attempt)(void);
  __block __weak void (^weakAttempt)(void);
  attempt = ^{
    NSRunningApplication *app = [NSRunningApplication runningApplicationsWithBundleIdentifier:bid].firstObject;
    CGWindowID wid = app ? frontWindowOf(app.processIdentifier) : 0;
    if (wid) {
      [SCShareableContent getShareableContentExcludingDesktopWindows:YES onScreenWindowsOnly:YES
                                                   completionHandler:^(SCShareableContent *content, NSError *err) {
        SCWindow *target = nil;
        for (SCWindow *w in content.windows) if (w.windowID == wid) target = w;
        dispatch_async(dispatch_get_main_queue(), ^{
          setStatus(nil);
          if (target) openWindow(target);
        });
      }];
      return;
    }
    if (tries == 0 && url) {
      NSWorkspaceOpenConfiguration *cfg = [NSWorkspaceOpenConfiguration configuration];
      cfg.activates = YES;
      [NSWorkspace.sharedWorkspace openApplicationAtURL:url configuration:cfg completionHandler:nil];
    }
    if (++tries > 16) {
      setStatus([NSString stringWithFormat:@"%@ has no window to show", name]);
      return;
    }
    void (^again)(void) = weakAttempt;
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(0.5 * NSEC_PER_SEC)), dispatch_get_main_queue(), again);
  };
  weakAttempt = attempt;
  setStatus([NSString stringWithFormat:@"Opening %@…", name]);
  attempt();
  // Keep the block alive while it reschedules itself.
  dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(9 * NSEC_PER_SEC)), dispatch_get_main_queue(), ^{ attempt = nil; });
}

static void togglePin(NSDictionary *e) {
  NSString *bid = e[@"bid"];
  if ([gPins containsObject:bid]) [gPins removeObject:bid];
  else [gPins addObject:bid];
  savePins();
  redrawPicker(false);
}

// Search typed on the tablet's keyboard while the list is shown.
static void pickerKey(uint16_t code) {
  static const char *row1 = "qwertyuiop", *row2 = "asdfghjkl", *row3 = "zxcvbnm", *digits = "1234567890";
  char c = 0;
  if (code >= 16 && code <= 25) c = row1[code - 16];
  else if (code >= 30 && code <= 38) c = row2[code - 30];
  else if (code >= 44 && code <= 50) c = row3[code - 44];
  else if (code >= 2 && code <= 11) c = digits[code - 2];
  else if (code == 57) c = ' ';
  else if (code == 12) c = '-';
  else if (code == 52) c = '.';
  if (c) gQuery = [gQuery stringByAppendingFormat:@"%c", c];
  else if (code == 14 && gQuery.length) gQuery = [gQuery substringToIndex:gQuery.length - 1];  // Backspace
  else if (code == 1) gQuery = @"";                                                            // Esc
  else if (code == 28) {                                                                       // Enter
    pthread_mutex_lock(&gLock);
    NSDictionary *first = gEntries.firstObject;
    pthread_mutex_unlock(&gLock);
    if (first) openApp(first);
    return;
  } else return;
  gStatus = nil;
  redrawPicker(true);
}

static void openWindow(SCWindow *w) {
  stopCapture();
  CGRect content = contentRectFor(w.frame);
  pthread_mutex_lock(&gLock);
  gWindow = w;
  gWinFrame = w.frame;
  gContent = content;
  gMode = kModeWindow;
  drawWindowChromeLocked();
  pthread_mutex_unlock(&gLock);
  SCContentFilter *filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:w];
  gCapture.stream = [[SCStream alloc] initWithFilter:filter configuration:configFor(content) delegate:gCapture];
  NSError *err = nil;
  [gCapture.stream addStreamOutput:gCapture type:SCStreamOutputTypeScreen
                sampleHandlerQueue:dispatch_queue_create("capture", DISPATCH_QUEUE_SERIAL) error:&err];
  if (err) { fprintf(stderr, "rmscreen: %s\n", err.localizedDescription.UTF8String); return; }
  [gCapture.stream startCaptureWithCompletionHandler:^(NSError *se) {
    if (se) fprintf(stderr, "rmscreen: capture failed: %s\n", se.localizedDescription.UTF8String);
  }];
  fprintf(stderr, "rmscreen: mirroring %s\n", windowLabel(w).UTF8String);
  restoreOriginal();  // in case another window was resized before
  resizeForTablet(w);
  dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(0.3 * NSEC_PER_SEC)), dispatch_get_main_queue(), ^{ trackWindow(); });
}

// Follows the mirrored window's size and position; refits when it changes size.
static void trackWindow(void) {
  pthread_mutex_lock(&gLock);
  SCWindow *w = gMode == kModeWindow ? gWindow : nil;
  CGRect old = gWinFrame;
  pthread_mutex_unlock(&gLock);
  if (!w) return;
  CFArrayRef info = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, w.windowID);
  if (!info || CFArrayGetCount(info) == 0) {  // window closed
    if (info) CFRelease(info);
    showPicker();
    return;
  }
  CGRect frame = CGRectZero;
  CFDictionaryRef bounds = CFDictionaryGetValue(CFArrayGetValueAtIndex(info, 0), kCGWindowBounds);
  if (bounds) CGRectMakeWithDictionaryRepresentation(bounds, &frame);
  CFRelease(info);
  if (CGRectIsEmpty(frame) || CGRectEqualToRect(frame, old)) return;
  bool resized = !CGSizeEqualToSize(frame.size, old.size);
  pthread_mutex_lock(&gLock);
  gWinFrame = frame;
  if (resized) {
    gContent = contentRectFor(frame);
    drawWindowChromeLocked();
  }
  CGRect content = gContent;
  pthread_mutex_unlock(&gLock);
  if (resized) [gCapture.stream updateConfiguration:configFor(content) completionHandler:nil];
}

// ---- Orientation (main thread) ----

static void setOrientation(bool landscape) {
  int w = landscape ? kPixH : kPixW, h = landscape ? kPixW : kPixH;
  pthread_mutex_lock(&gLock);
  bool same = w == gW && h == gH;
  if (!same) {
    gW = w;
    gH = h;
    gResizeSeq++;
  }
  int mode = gMode;
  SCWindow *win = gWindow;
  if (!same && mode == kModeWindow) {
    gContent = contentRectFor(gWinFrame);
    drawWindowChromeLocked();
  }
  CGRect content = gContent;
  pthread_mutex_unlock(&gLock);
  if (same) return;
  fprintf(stderr, "rmscreen: orientation %s\n", landscape ? "landscape" : "portrait");
  if (mode == kModeWindow && win) {
    [gCapture.stream updateConfiguration:configFor(content) completionHandler:nil];
    resizeForTablet(win);
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(0.3 * NSEC_PER_SEC)), dispatch_get_main_queue(), ^{ trackWindow(); });
  } else {
    showPicker();
  }
}

// ---- Input on the mirrored window ----

static bool toWindowPoint(int x, int y, CGPoint *out) {
  pthread_mutex_lock(&gLock);
  CGRect c = gContent, f = gWinFrame;
  pthread_mutex_unlock(&gLock);
  if (!CGRectContainsPoint(c, CGPointMake(x, y))) return false;
  *out = CGPointMake(f.origin.x + (x - c.origin.x) / c.size.width * f.size.width,
                     f.origin.y + (y - c.origin.y) / c.size.height * f.size.height);
  return true;
}

// Brings the mirrored window's app and the window itself to the front, so a
// click lands on it rather than on whatever covers it on the Mac.
static void raiseMirrored(void) {
  pthread_mutex_lock(&gLock);
  SCWindow *w = gWindow;
  CGRect f = gWinFrame;
  pthread_mutex_unlock(&gLock);
  if (!w) return;
  pid_t pid = w.owningApplication.processID;
  AXUIElementRef app = AXUIElementCreateApplication(pid);
  AXUIElementSetAttributeValue(app, kAXFrontmostAttribute, kCFBooleanTrue);
  CFRelease(app);
  AXUIElementRef win = copyAXWindow(pid, f);
  if (win) {
    AXUIElementPerformAction(win, kAXRaiseAction);
    CFRelease(win);
  }
}

static void postMouse(CGEventType type, CGPoint p, CGMouseButton btn) {
  CGEventRef e = CGEventCreateMouseEvent(NULL, type, p, btn);
  CGEventPost(kCGHIDEventTap, e);
  CFRelease(e);
}

static CGPoint currentPointer(void) {
  CGEventRef e = CGEventCreate(NULL);
  CGPoint p = CGEventGetLocation(e);
  CFRelease(e);
  return p;
}

static void clickAt(CGPoint p, bool right) {
  CGPoint home = currentPointer();
  raiseMirrored();
  usleep(60 * 1000);  // let the raise land before clicking
  CGMouseButton btn = right ? kCGMouseButtonRight : kCGMouseButtonLeft;
  postMouse(kCGEventMouseMoved, p, kCGMouseButtonLeft);
  postMouse(right ? kCGEventRightMouseDown : kCGEventLeftMouseDown, p, btn);
  postMouse(right ? kCGEventRightMouseUp : kCGEventLeftMouseUp, p, btn);
  usleep(30 * 1000);
  CGWarpMouseCursorPosition(home);
  CGAssociateMouseAndMouseCursorPosition(true);
}

static void scrollAt(CGPoint p, int dyPixels, int dxPixels, CGFloat pointsPerPixel) {
  CGEventRef e = CGEventCreateScrollWheelEvent(NULL, kCGScrollEventUnitPixel, 2,
                                               (int32_t)lround(dyPixels * pointsPerPixel),
                                               (int32_t)lround(dxPixels * pointsPerPixel));
  CGEventSetLocation(e, p);
  CGEventPost(kCGHIDEventTap, e);
  CFRelease(e);
}

// One-finger touch from the viewer is the mouse: a tap clicks, a drag drags
// (selects, moves), a long press right-clicks. In the list, a tap opens an app
// and a long press pins it. Two-finger scrolling arrives separately (251).
static struct {
  bool down, dragging;
  int startX, startY;
  double startTime;
  CGPoint startMac, home;
} gTouch;

static double now(void) { return CFAbsoluteTimeGetCurrent(); }

static NSDictionary *entryAt(int y) {
  pthread_mutex_lock(&gLock);
  NSInteger row = y < kHeaderH ? -1 : (NSInteger)((y - kHeaderH + gScroll) / kRowH);
  NSDictionary *e = row >= 0 && row < (NSInteger)gEntries.count ? gEntries[row] : nil;
  pthread_mutex_unlock(&gLock);
  return e;
}

static void handlePointer(uint8_t mask, uint16_t x, uint16_t y, uint8_t *prevMask) {
  bool isDown = mask & 1, wasDown = *prevMask & 1, cancel = mask & 0x80;
  *prevMask = mask & ~0x80;
  pthread_mutex_lock(&gLock);
  bool picker = gMode == kModePicker;
  pthread_mutex_unlock(&gLock);

  if (cancel) {  // a second finger arrived: this was the start of a scroll
    if (gTouch.dragging) {
      CGPoint at = gTouch.startMac, home = gTouch.home;
      dispatch_async(dispatch_get_main_queue(), ^{
        postMouse(kCGEventLeftMouseUp, at, kCGMouseButtonLeft);
        CGWarpMouseCursorPosition(home);
        CGAssociateMouseAndMouseCursorPosition(true);
      });
    }
    gTouch.down = gTouch.dragging = false;
    return;
  }
  if (isDown && !wasDown) {
    gTouch.down = true;
    gTouch.dragging = false;
    gTouch.startX = x;
    gTouch.startY = y;
    gTouch.startTime = now();
    return;
  }
  if (!gTouch.down) return;
  bool ui = picker || gTouch.startY < kBarH;
  CGPoint start, cur;
  bool inWindow = !ui && toWindowPoint(gTouch.startX, gTouch.startY, &start);

  if (isDown) {  // finger moving
    if (!inWindow) return;
    if (!toWindowPoint(x, y, &cur)) return;
    if (!gTouch.dragging && (abs(x - gTouch.startX) > kDragThreshold || abs(y - gTouch.startY) > kDragThreshold)) {
      gTouch.dragging = true;
      gTouch.startMac = start;
      dispatch_async(dispatch_get_main_queue(), ^{
        gTouch.home = currentPointer();
        raiseMirrored();
        usleep(60 * 1000);
        postMouse(kCGEventMouseMoved, start, kCGMouseButtonLeft);
        postMouse(kCGEventLeftMouseDown, start, kCGMouseButtonLeft);
        postMouse(kCGEventLeftMouseDragged, cur, kCGMouseButtonLeft);
      });
    } else if (gTouch.dragging) {
      gTouch.startMac = cur;
      dispatch_async(dispatch_get_main_queue(), ^{ postMouse(kCGEventLeftMouseDragged, cur, kCGMouseButtonLeft); });
    }
    return;
  }

  // Finger lifted.
  gTouch.down = false;
  bool longPress = now() - gTouch.startTime >= kLongPress;
  if (gTouch.dragging) {
    gTouch.dragging = false;
    CGPoint end = gTouch.startMac, home = gTouch.home;
    if (toWindowPoint(x, y, &cur)) end = cur;
    dispatch_async(dispatch_get_main_queue(), ^{
      postMouse(kCGEventLeftMouseUp, end, kCGMouseButtonLeft);
      usleep(30 * 1000);
      CGWarpMouseCursorPosition(home);
      CGAssociateMouseAndMouseCursorPosition(true);
    });
    return;
  }
  if (picker) {
    NSDictionary *e = entryAt(gTouch.startY);
    if (e) dispatch_async(dispatch_get_main_queue(), ^{ longPress ? togglePin(e) : openApp(e); });
    return;
  }
  if (ui) {  // the bar: back to the list
    dispatch_async(dispatch_get_main_queue(), ^{ showPicker(); });
    return;
  }
  if (inWindow) dispatch_async(dispatch_get_main_queue(), ^{ clickAt(start, longPress); });
}

// Two-finger scroll from the viewer: logical pixels, positive dy = fingers moved down.
static void handleScroll(int x, int y, int dx, int dy) {
  pthread_mutex_lock(&gLock);
  bool picker = gMode == kModePicker;
  CGFloat pointsPerPixel = gContent.size.width > 0 ? gWinFrame.size.width / gContent.size.width : 1;
  if (picker) {
    gScroll -= dy;
    drawPickerLocked();
  }
  pthread_mutex_unlock(&gLock);
  CGPoint at;
  if (!picker && toWindowPoint(x, y, &at)) {
    dispatch_async(dispatch_get_main_queue(), ^{ scrollAt(at, dy, dx, pointsPerPixel); });
  }
}

static int keysymToKeycode(uint32_t ks) {
  switch (ks) {
    case 0xff0d: return 36;  // Return
    case 0xff08: return 51;  // BackSpace
    case 0xff09: return 48;  // Tab
    case 0xff1b: return 53;  // Escape
    case 0xffff: return 117; // Delete
    case 0xff51: return 123; // Left
    case 0xff53: return 124; // Right
    case 0xff54: return 125; // Down
    case 0xff52: return 126; // Up
    case 0xff50: return 115; // Home
    case 0xff57: return 119; // End
    case 0xff55: return 116; // Page Up
    case 0xff56: return 121; // Page Down
    default: return -1;
  }
}

static void handleLinuxKey(uint16_t code, uint8_t value);

static void handleKey(bool down, uint32_t ks) {
  if ((ks & 0xfff00000) == 0x10f00000) {  // raw Linux keycode from the tablet viewer
    handleLinuxKey((uint16_t)(ks & 0xffff), down ? 1 : 0);
    return;
  }
  int code = keysymToKeycode(ks);
  CGEventRef e;
  if (code >= 0) {
    e = CGEventCreateKeyboardEvent(NULL, (CGKeyCode)code, down);
  } else if (ks >= 0x20 && ks <= 0x7e) {
    if (!down) return;  // typed as a unicode string on key down
    UniChar ch = (UniChar)ks;
    e = CGEventCreateKeyboardEvent(NULL, 0, true);
    CGEventKeyboardSetUnicodeString(e, 1, &ch);
    CGEventPost(kCGHIDEventTap, e);
    CFRelease(e);
    e = CGEventCreateKeyboardEvent(NULL, 0, false);
    CGEventKeyboardSetUnicodeString(e, 1, &ch);
  } else {
    return;
  }
  CGEventPost(kCGHIDEventTap, e);
  CFRelease(e);
}

// ---- Socket helpers ----

static bool readAll(int fd, void *buf, size_t n) {
  uint8_t *p = buf;
  while (n) {
    ssize_t r = read(fd, p, n);
    if (r <= 0) return false;
    p += r; n -= (size_t)r;
  }
  return true;
}

static bool writeAll(int fd, const void *buf, size_t n) {
  const uint8_t *p = buf;
  while (n) {
    ssize_t r = write(fd, p, n);
    if (r <= 0) return false;
    p += r; n -= (size_t)r;
  }
  return true;
}

static bool peerAllowed(struct sockaddr_in *a) {
  in_addr_t ip = a->sin_addr.s_addr;
  // The tablet only (plus this Mac itself, for testing).
  return ip == inet_addr("10.11.99.1") || ip == gTabletTailscaleIP || ip == inet_addr("127.0.0.1");
}


// ---- Key channel: physical keyboard on the tablet (port 5901) ----
//
// rmkeys on the tablet sends 3-byte events: Linux keycode (BE u16) and value
// (0 up, 1 down, 2 repeat). Keys go to the mirrored window's app, which is
// brought to the front first so typing never lands anywhere else.

static int linuxToMac(uint16_t c) {
  static const int16_t map[128] = {
    [1] = 53, [2] = 18, [3] = 19, [4] = 20, [5] = 21, [6] = 23, [7] = 22, [8] = 26, [9] = 28, [10] = 25,
    [11] = 29, [12] = 27, [13] = 24, [14] = 51, [15] = 48, [16] = 12, [17] = 13, [18] = 14, [19] = 15,
    [20] = 17, [21] = 16, [22] = 32, [23] = 34, [24] = 31, [25] = 35, [26] = 33, [27] = 30, [28] = 36,
    [29] = 59, [30] = 0, [31] = 1, [32] = 2, [33] = 3, [34] = 5, [35] = 4, [36] = 38, [37] = 40,
    [38] = 37, [39] = 41, [40] = 39, [41] = 50, [42] = 56, [43] = 42, [44] = 6, [45] = 7, [46] = 8,
    [47] = 9, [48] = 11, [49] = 45, [50] = 46, [51] = 43, [52] = 47, [53] = 44, [54] = 60, [56] = 58,
    [57] = 49, [58] = 57, [59] = 122, [60] = 120, [61] = 99, [62] = 118, [63] = 96, [64] = 97, [65] = 98,
    [66] = 100, [67] = 101, [68] = 109, [86] = 10, [87] = 103, [88] = 111, [96] = 76, [97] = 62,
    [100] = 61, [102] = 115, [103] = 126, [104] = 116, [105] = 123, [106] = 124, [107] = 119,
    [108] = 125, [109] = 121, [111] = 117, [125] = 55, [126] = 54,
  };
  if (c >= 128) return -1;
  int m = map[c];
  return (m == 0 && c != 30) ? -1 : m;  // 'a' is the only key that maps to 0
}

static CGEventFlags modifierFlag(uint16_t c) {
  switch (c) {
    case 42: case 54: return kCGEventFlagMaskShift;
    case 29: case 97: return kCGEventFlagMaskControl;
    case 56: case 100: return kCGEventFlagMaskAlternate;
    case 125: case 126: return kCGEventFlagMaskCommand;
    default: return 0;
  }
}

static void ensureMirroredFront(void) {
  pthread_mutex_lock(&gLock);
  pid_t pid = gWindow ? gWindow.owningApplication.processID : 0;
  pthread_mutex_unlock(&gLock);
  if (pid && NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier != pid) {
    raiseMirrored();
    usleep(80 * 1000);
  }
}

// value: 0 up, 1 down, 2 repeat. Modifier state is shared by both key paths.
static CGEventFlags gKeyFlags;
static bool gCaps;

static void handleLinuxKey(uint16_t code, uint8_t value) {
  pthread_mutex_lock(&gLock);
  bool mirroring = gMode == kModeWindow;
  pthread_mutex_unlock(&gLock);
  CGEventFlags mod = modifierFlag(code);
  if (mod) {
    gKeyFlags = value ? (gKeyFlags | mod) : (gKeyFlags & ~mod);
    return;
  }
  if (code == 58) {  // Caps Lock toggles on press
    if (value == 1) gCaps = !gCaps;
    return;
  }
  if (!mirroring) {
    if (value != 0 && !(gKeyFlags & (kCGEventFlagMaskCommand | kCGEventFlagMaskControl))) {
      dispatch_async(dispatch_get_main_queue(), ^{ pickerKey(code); });
    }
    return;
  }
  int mac = linuxToMac(code);
  if (mac < 0) return;
  if (value == 1) dispatch_sync(dispatch_get_main_queue(), ^{ ensureMirroredFront(); });
  CGEventRef e = CGEventCreateKeyboardEvent(NULL, (CGKeyCode)mac, value != 0);
  CGEventSetFlags(e, gKeyFlags | (gCaps ? kCGEventFlagMaskAlphaShift : 0));
  if (value == 2) CGEventSetIntegerValueField(e, kCGKeyboardEventAutorepeat, 1);
  CGEventPost(kCGHIDEventTap, e);
  CFRelease(e);
}

static void serveKeys(int fd) {
  uint8_t b[3];
  while (readAll(fd, b, 3)) handleLinuxKey((uint16_t)(b[0] << 8 | b[1]), b[2]);
}

struct Conn {
  int fd;
  void (*serve)(int);
  int *active;  // the listener's current connection, guarded by gConnLock
};

static pthread_mutex_t gConnLock = PTHREAD_MUTEX_INITIALIZER;

static void *connThread(void *arg) {
  struct Conn c = *(struct Conn *)arg;
  free(arg);
  c.serve(c.fd);
  // Forget this connection before closing it, so its fd number (which the
  // next accept may reuse) is never shut down by mistake.
  pthread_mutex_lock(&gConnLock);
  if (*c.active == c.fd) *c.active = -1;
  close(c.fd);
  pthread_mutex_unlock(&gConnLock);
  return NULL;
}

// Accepts the tablet's connections. The newest one wins: an older session on
// the same port is shut down, so reopening Mac Screen never waits behind a
// stale connection.
static void listenOn(int port, void (*serve)(int), const char *what) {
  int srv = socket(AF_INET, SOCK_STREAM, 0);
  int one = 1;
  setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  struct sockaddr_in addr = {.sin_family = AF_INET, .sin_port = htons(port), .sin_addr.s_addr = htonl(INADDR_ANY)};
  if (bind(srv, (struct sockaddr *)&addr, sizeof addr) != 0 || listen(srv, 4) != 0) {
    die([NSString stringWithFormat:@"cannot listen on port %d", port]);
  }
  fprintf(stderr, "rmscreen: %s on port %d (tablet only: USB and Tailscale)\n", what, port);
  int *active = malloc(sizeof *active);
  *active = -1;
  for (;;) {
    struct sockaddr_in peer;
    socklen_t plen = sizeof peer;
    int fd = accept(srv, (struct sockaddr *)&peer, &plen);
    if (fd < 0) continue;
    if (!peerAllowed(&peer)) {
      fprintf(stderr, "rmscreen: refused %s\n", inet_ntoa(peer.sin_addr));
      close(fd);
      continue;
    }
    int nosig = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &nosig, sizeof nosig);
    pthread_mutex_lock(&gConnLock);
    if (*active >= 0) shutdown(*active, SHUT_RDWR);  // ends the older session
    *active = fd;
    pthread_mutex_unlock(&gConnLock);
    struct Conn *c = malloc(sizeof *c);
    c->fd = fd;
    c->serve = serve;
    c->active = active;
    pthread_t t;
    pthread_create(&t, NULL, connThread, c);
    pthread_detach(t);
  }
}

// ---- Pixel format conversion ----

typedef struct {
  uint8_t bpp, depth, bigEndian, trueColor;
  uint16_t rMax, gMax, bMax;
  uint8_t rShift, gShift, bShift;
} PixFmt;

static size_t convertRect(const PixFmt *f, const uint8_t *frame, int x, int y, int w, int h, uint8_t *out) {
  size_t bytes = f->bpp / 8, o = 0;
  for (int row = y; row < y + h; row++) {
    const uint8_t *src = frame + ((size_t)row * gW + x) * 4;
    for (int col = 0; col < w; col++, src += 4) {
      uint32_t b = src[0], g = src[1], r = src[2];
      uint32_t v = ((r * f->rMax + 127) / 255) << f->rShift |
                   ((g * f->gMax + 127) / 255) << f->gShift |
                   ((b * f->bMax + 127) / 255) << f->bShift;
      for (size_t i = 0; i < bytes; i++) {
        size_t shift = f->bigEndian ? (bytes - 1 - i) * 8 : i * 8;
        out[o++] = (uint8_t)(v >> shift);
      }
    }
  }
  return o;
}

// ---- RFB session ----

typedef struct {
  int fd;
  PixFmt fmt;
  bool wantUpdate, incremental;
  int rx, ry, rw, rh;        // requested region
  uint8_t *sent;             // last frame sent to this client (BGRA)
  uint64_t sentSeq;
  bool alive;
  uint64_t resizeSeq;        // last logical size announced to this client
  bool desktopSize;          // client accepts DesktopSize (size changes)
  bool zlib;                 // client accepts Zlib encoding (one stream per connection)
  bool zInit;
  z_stream z;
} Session;

static const double kMinUpdateInterval = 0.5;  // seconds; e-ink and the viewer need breathing room

// Returns true if an update was sent. A request with nothing new to send stays
// open: the viewer waits for an answer before it asks again.
static bool sendUpdate(Session *s, bool full) {
  // Collect dirty tiles inside the requested region, merged per tile row.
  int tilesX = (gW + kTile - 1) / kTile, tilesY = (gH + kTile - 1) / kTile;
  typedef struct { int x, y, w, h; } Rect;
  Rect *rects = malloc(sizeof(Rect) * (size_t)tilesX * (size_t)tilesY);
  int n = 0;
  pthread_mutex_lock(&gLock);
  uint64_t seq = gFrameSeq;
  bool resize = s->desktopSize && s->resizeSeq != gResizeSeq;
  if (resize) {
    full = true;
    s->rx = 0; s->ry = 0; s->rw = gW; s->rh = gH;
    memset(s->sent, 0, (size_t)kPixW * kPixH * 4);
  }
  for (int ty = 0; ty < tilesY; ty++) {
    int runStart = -1;
    for (int tx = 0; tx <= tilesX; tx++) {
      bool dirty = false;
      if (tx < tilesX) {
        int x0 = tx * kTile, y0 = ty * kTile;
        int w = MIN(kTile, gW - x0), h = MIN(kTile, gH - y0);
        bool inReq = x0 + w > s->rx && x0 < s->rx + s->rw && y0 + h > s->ry && y0 < s->ry + s->rh;
        if (inReq) {
          if (full) dirty = true;
          else for (int y = y0; y < y0 + h && !dirty; y++) {
            size_t off = ((size_t)y * gW + x0) * 4;
            dirty = memcmp(gFrame + off, s->sent + off, (size_t)w * 4) != 0;
          }
        }
      }
      if (dirty && runStart < 0) runStart = tx;
      if (!dirty && runStart >= 0) {
        int x0 = runStart * kTile, y0 = ty * kTile;
        rects[n++] = (Rect){x0, y0, MIN(tx * kTile, gW) - x0, MIN(kTile, gH - y0)};
        runStart = -1;
      }
    }
  }
  s->sentSeq = seq;
  if (n == 0) {
    pthread_mutex_unlock(&gLock);
    free(rects);
    return false;
  }
  size_t bytesPer = s->fmt.bpp / 8, total = 4 + 12, maxRaw = 0;
  for (int i = 0; i < n; i++) {
    size_t raw = (size_t)rects[i].w * rects[i].h * bytesPer;
    maxRaw = MAX(maxRaw, raw);
    total += 16 + (s->zlib ? deflateBound(&s->z, raw) + 64 : raw);
  }
  uint8_t *msg = malloc(total), *p = msg;
  uint8_t *raw = s->zlib ? malloc(maxRaw) : NULL;
  *p++ = 0; *p++ = 0;  // FramebufferUpdate, padding
  int count = n + (resize ? 1 : 0);
  *p++ = (uint8_t)(count >> 8); *p++ = (uint8_t)count;
  if (resize) {  // DesktopSize pseudo-rectangle: the new logical size
    uint16_t hdr[4] = {0, 0, htons(gW), htons(gH)};
    memcpy(p, hdr, 8); p += 8;
    uint32_t enc = htonl((uint32_t)-223);
    memcpy(p, &enc, 4); p += 4;
    s->resizeSeq = gResizeSeq;
  }
  for (int i = 0; i < n; i++) {
    Rect r = rects[i];
    uint16_t hdr[4] = {htons(r.x), htons(r.y), htons(r.w), htons(r.h)};
    memcpy(p, hdr, 8); p += 8;
    if (s->zlib) {
      // Zlib encoding: U32 length, then this rect's pixels through the
      // connection's single deflate stream, sync-flushed.
      uint32_t enc = htonl(6);
      memcpy(p, &enc, 4); p += 4;
      size_t len = convertRect(&s->fmt, gFrame, r.x, r.y, r.w, r.h, raw);
      uint8_t *lenAt = p;
      p += 4;
      s->z.next_in = raw;
      s->z.avail_in = (uInt)len;
      s->z.next_out = p;
      s->z.avail_out = (uInt)(msg + total - p);
      deflate(&s->z, Z_SYNC_FLUSH);
      uint32_t clen = (uint32_t)(s->z.next_out - p);
      uint32_t be = htonl(clen);
      memcpy(lenAt, &be, 4);
      p += clen;
    } else {
      uint32_t enc = htonl(0);  // Raw
      memcpy(p, &enc, 4); p += 4;
      p += convertRect(&s->fmt, gFrame, r.x, r.y, r.w, r.h, p);
    }
    for (int y = r.y; y < r.y + r.h; y++) {
      size_t off = ((size_t)y * gW + r.x) * 4;
      memcpy(s->sent + off, gFrame + off, (size_t)r.w * 4);
    }
  }
  pthread_mutex_unlock(&gLock);
  if (!writeAll(s->fd, msg, (size_t)(p - msg))) s->alive = false;
  pthread_mutex_lock(&gLock);
  gStatUpdates++;
  gStatBytes += (uint64_t)(p - msg);
  pthread_mutex_unlock(&gLock);
  free(raw);
  free(msg);
  free(rects);
  return true;
}

static void *updateLoop(void *arg) {
  Session *s = arg;
  while (s->alive) {
    pthread_mutex_lock(&gLock);
    while (s->alive && !(s->wantUpdate && (!s->incremental || gFrameSeq != s->sentSeq ||
                                           (s->desktopSize && s->resizeSeq != gResizeSeq)))) {
      struct timespec ts;
      clock_gettime(CLOCK_REALTIME, &ts);
      ts.tv_nsec += 200 * 1000 * 1000;
      if (ts.tv_nsec >= 1000000000) { ts.tv_sec++; ts.tv_nsec -= 1000000000; }
      pthread_cond_timedwait(&gFrameCond, &gLock, &ts);
    }
    bool full = !s->incremental;
    s->wantUpdate = false;
    pthread_mutex_unlock(&gLock);
    if (!s->alive) break;
    double started = CFAbsoluteTimeGetCurrent();
    if (!sendUpdate(s, full)) {
      pthread_mutex_lock(&gLock);
      s->wantUpdate = true;  // nothing changed: keep the request open
      pthread_mutex_unlock(&gLock);
      continue;
    }
    double rest = kMinUpdateInterval - (CFAbsoluteTimeGetCurrent() - started);
    if (rest > 0) usleep((useconds_t)(rest * 1e6));
  }
  return NULL;
}

static void serveClient(int fd) {
  int one = 1;
  setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
  setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);  // a closed viewer must not kill us
  if (!writeAll(fd, "RFB 003.008\n", 12)) return;
  char ver[12];
  if (!readAll(fd, ver, 12)) return;
  uint8_t types[2] = {1, 1};  // one type: None (access is limited by peer address)
  if (!writeAll(fd, types, 2)) return;
  uint8_t chosen;
  if (!readAll(fd, &chosen, 1) || chosen != 1) return;
  uint32_t result = htonl(0);
  if (!writeAll(fd, &result, 4)) return;
  uint8_t shared;
  if (!readAll(fd, &shared, 1)) return;

  Session s = {.fd = fd, .alive = true};
  pthread_mutex_lock(&gLock);
  s.resizeSeq = gResizeSeq;
  pthread_mutex_unlock(&gLock);
  s.fmt = (PixFmt){32, 24, 0, 1, 255, 255, 255, 16, 8, 0};
  s.sent = calloc((size_t)kPixW * kPixH, 4);

  const char *name = "Mac for reMarkable";
  uint8_t init[24];
  uint16_t wh[2] = {htons(gW), htons(gH)};
  memcpy(init, wh, 4);
  init[4] = 32; init[5] = 24; init[6] = 0; init[7] = 1;
  uint16_t maxes[3] = {htons(255), htons(255), htons(255)};
  memcpy(init + 8, maxes, 6);
  init[14] = 16; init[15] = 8; init[16] = 0;
  memset(init + 17, 0, 3);
  uint32_t nlen = htonl((uint32_t)strlen(name));
  memcpy(init + 20, &nlen, 4);
  if (!writeAll(fd, init, 24) || !writeAll(fd, name, strlen(name))) { free(s.sent); return; }
  fprintf(stderr, "rmscreen: client connected\n");
  dispatch_async(dispatch_get_main_queue(), ^{ showPicker(); });

  pthread_t updater;
  pthread_create(&updater, NULL, updateLoop, &s);
  uint8_t prevMask = 0;
  while (s.alive) {
    uint8_t type;
    if (!readAll(fd, &type, 1)) break;
    if (type == 0) {  // SetPixelFormat
      uint8_t b[19];
      if (!readAll(fd, b, 19)) break;
      PixFmt f = {b[3], b[4], b[5], b[6], (uint16_t)(b[7] << 8 | b[8]), (uint16_t)(b[9] << 8 | b[10]),
                  (uint16_t)(b[11] << 8 | b[12]), b[13], b[14], b[15]};
      if (!f.trueColor || (f.bpp != 8 && f.bpp != 16 && f.bpp != 32)) { fprintf(stderr, "rmscreen: unsupported pixel format\n"); break; }
      pthread_mutex_lock(&gLock);
      s.fmt = f;
      memset(s.sent, 0, (size_t)kPixW * kPixH * 4);
      pthread_mutex_unlock(&gLock);
    } else if (type == 2) {  // SetEncodings: use Zlib when offered, else Raw
      uint8_t b[3];
      if (!readAll(fd, b, 3)) break;
      uint16_t count = (uint16_t)(b[1] << 8 | b[2]);
      uint8_t *list = malloc((size_t)count * 4 + 1);
      bool r = readAll(fd, list, (size_t)count * 4);
      bool zlib = false;
      for (uint16_t i = 0; r && i < count; i++) {
        int32_t enc = (int32_t)((uint32_t)list[i * 4] << 24 | (uint32_t)list[i * 4 + 1] << 16 |
                                (uint32_t)list[i * 4 + 2] << 8 | list[i * 4 + 3]);
        if (enc == 6) zlib = true;
        if (enc == -223) s.desktopSize = true;
      }
      free(list);
      if (!r) break;
      pthread_mutex_lock(&gLock);
      if (zlib && !s.zInit) {
        s.zInit = deflateInit(&s.z, 6) == Z_OK;
      }
      s.zlib = zlib && s.zInit;
      pthread_mutex_unlock(&gLock);
      fprintf(stderr, "rmscreen: encoding %s\n", s.zlib ? "zlib" : "raw");
    } else if (type == 3) {  // FramebufferUpdateRequest
      uint8_t b[9];
      if (!readAll(fd, b, 9)) break;
      pthread_mutex_lock(&gLock);
      s.incremental = b[0];
      s.rx = b[1] << 8 | b[2]; s.ry = b[3] << 8 | b[4];
      s.rw = b[5] << 8 | b[6]; s.rh = b[7] << 8 | b[8];
      s.wantUpdate = true;
      pthread_cond_broadcast(&gFrameCond);
      pthread_mutex_unlock(&gLock);
    } else if (type == 4) {  // KeyEvent
      uint8_t b[7];
      if (!readAll(fd, b, 7)) break;
      handleKey(b[0], (uint32_t)b[3] << 24 | (uint32_t)b[4] << 16 | (uint32_t)b[5] << 8 | b[6]);
    } else if (type == 5) {  // PointerEvent
      uint8_t b[5];
      if (!readAll(fd, b, 5)) break;
      handlePointer(b[0], (uint16_t)(b[1] << 8 | b[2]), (uint16_t)(b[3] << 8 | b[4]), &prevMask);
    } else if (type == 251) {  // our viewer: two-finger scroll
      uint8_t b[9];
      if (!readAll(fd, b, 9)) break;
      handleScroll(b[1] << 8 | b[2], b[3] << 8 | b[4], (int16_t)(b[5] << 8 | b[6]), (int16_t)(b[7] << 8 | b[8]));
    } else if (type == 250) {  // our viewer: orientation (0 portrait, 1 landscape)
      uint8_t o;
      if (!readAll(fd, &o, 1)) break;
      dispatch_async(dispatch_get_main_queue(), ^{ setOrientation(o == 1); });
    } else if (type == 6) {  // ClientCutText
      uint8_t b[7];
      if (!readAll(fd, b, 7)) break;
      uint32_t len = (uint32_t)b[3] << 24 | (uint32_t)b[4] << 16 | (uint32_t)b[5] << 8 | b[6];
      uint8_t *skip = malloc(len ? len : 1);
      bool r = readAll(fd, skip, len);
      free(skip);
      if (!r) break;
    } else {
      fprintf(stderr, "rmscreen: unknown client message %d\n", type);
      break;
    }
  }
  pthread_mutex_lock(&gLock);
  s.alive = false;
  pthread_cond_broadcast(&gFrameCond);
  pthread_mutex_unlock(&gLock);
  pthread_join(updater, NULL);
  if (s.zInit) deflateEnd(&s.z);
  free(s.sent);
  dispatch_async(dispatch_get_main_queue(), ^{ showPicker(); });  // also restores the window
  fprintf(stderr, "rmscreen: client disconnected\n");
}

// ---- Main ----

int main(int argc, const char *argv[]) {
  @autoreleasepool {
    if (argc != 2) die(@"usage: rmscreen <tablet-tailscale-ip>");
    signal(SIGPIPE, SIG_IGN);
    gTabletTailscaleIP = inet_addr(argv[1]);
    if (gTabletTailscaleIP == INADDR_NONE) die(@"invalid tablet address");

    // Set up as a (background) app: this opens the window-server connection
    // that window capture and input injection rely on.
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];

    if (!CGPreflightScreenCaptureAccess()) {
      CGRequestScreenCaptureAccess();
      fprintf(stderr, "rmscreen: needs Screen Recording permission (System Settings > Privacy & Security)\n");
    }
    NSDictionary *axOpts = @{(__bridge NSString *)kAXTrustedCheckOptionPrompt: @YES};
    if (!AXIsProcessTrustedWithOptions((__bridge CFDictionaryRef)axOpts)) {
      fprintf(stderr, "rmscreen: needs Accessibility permission for touch input\n");
    }

    gFrame = calloc((size_t)kPixW * kPixH, 4);
    gCapture = [Capture new];
    loadPins();
    scanInstalled();
    showPicker();
    [NSTimer scheduledTimerWithTimeInterval:60 repeats:YES block:^(NSTimer *t) { scanInstalled(); }];

    // Follow the mirrored window, and keep the picker current while it is shown.
    [NSTimer scheduledTimerWithTimeInterval:1 repeats:YES block:^(NSTimer *t) { trackWindow(); }];
    [NSTimer scheduledTimerWithTimeInterval:3 repeats:YES block:^(NSTimer *t) {
      pthread_mutex_lock(&gLock);
      bool picker = gMode == kModePicker;
      pthread_mutex_unlock(&gLock);
      if (picker) showPicker();
    }];

    [NSTimer scheduledTimerWithTimeInterval:3 repeats:YES block:^(NSTimer *t) {
      pthread_mutex_lock(&gLock);
      if (gStatFrames || gStatUpdates)  // quiet while idle
      fprintf(stderr, "rmscreen: stat mode=%s frames=%llu updates=%llu bytes=%llu content=%.0fx%.0f+%.0f+%.0f\n",
              gMode == kModePicker ? "picker" : "window", gStatFrames, gStatUpdates, gStatBytes,
              gContent.size.width, gContent.size.height, gContent.origin.x, gContent.origin.y);
      gStatFrames = gStatUpdates = gStatBytes = 0;
      pthread_mutex_unlock(&gLock);
    }];
    // SIGUSR1 saves what the tablet is being sent, for inspection.
    dispatch_source_t dump = dispatch_source_create(DISPATCH_SOURCE_TYPE_SIGNAL, SIGUSR1, 0, dispatch_get_main_queue());
    signal(SIGUSR1, SIG_IGN);
    dispatch_source_set_event_handler(dump, ^{
      pthread_mutex_lock(&gLock);
      CGContextRef ctx = frameContext();
      CGImageRef img = CGBitmapContextCreateImage(ctx);
      CGContextRelease(ctx);
      pthread_mutex_unlock(&gLock);
      NSString *path = [NSHomeDirectory() stringByAppendingPathComponent:@".config/rmscreen/frame.png"];
      CGImageDestinationRef dst = CGImageDestinationCreateWithURL((__bridge CFURLRef)[NSURL fileURLWithPath:path],
                                                                  CFSTR("public.png"), 1, NULL);
      CGImageDestinationAddImage(dst, img, NULL);
      CGImageDestinationFinalize(dst);
      CFRelease(dst);
      CGImageRelease(img);
      fprintf(stderr, "rmscreen: saved frame to %s\n", path.UTF8String);
    });
    dispatch_resume(dump);

    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
      listenOn(kPort, serveClient, "screen");
    });
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
      listenOn(kPort + 1, serveKeys, "keyboard");
    });
    [[NSRunLoop mainRunLoop] run];
  }
  return 0;
}
