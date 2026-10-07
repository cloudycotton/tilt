#!/usr/bin/env python3
"""Sets an image as the X root window's background, the way a desktop does: as a pixmap kept
after exit and published in _XROOTPMAP_ID, which xfwm4's compositor paints behind the windows.

    DISPLAY=:97 python3 e2e/demo/setroot.py wallpaper.png

Needs Pillow and libX11; the image should be the screen's size, on a 24-bit screen.
"""
import ctypes
import ctypes.util
import sys

from PIL import Image

x = ctypes.CDLL(ctypes.util.find_library('X11'))
x.XOpenDisplay.restype = ctypes.c_void_p
x.XOpenDisplay.argtypes = [ctypes.c_char_p]
x.XDefaultScreen.argtypes = [ctypes.c_void_p]
x.XRootWindow.restype = ctypes.c_ulong
x.XRootWindow.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XDefaultVisual.restype = ctypes.c_void_p
x.XDefaultVisual.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XDefaultDepth.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XCreatePixmap.restype = ctypes.c_ulong
x.XCreatePixmap.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_uint, ctypes.c_uint, ctypes.c_uint]
x.XCreateGC.restype = ctypes.c_void_p
x.XCreateGC.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_void_p]
x.XCreateImage.restype = ctypes.c_void_p
x.XCreateImage.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_int,
                           ctypes.c_char_p, ctypes.c_uint, ctypes.c_uint, ctypes.c_int, ctypes.c_int]
x.XPutImage.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_void_p, ctypes.c_void_p,
                        ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint]
x.XSetWindowBackgroundPixmap.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong]
x.XClearWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x.XInternAtom.restype = ctypes.c_ulong
x.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
x.XChangeProperty.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong,
                              ctypes.c_int, ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
x.XSetCloseDownMode.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XFlush.argtypes = [ctypes.c_void_p]
x.XCloseDisplay.argtypes = [ctypes.c_void_p]

ZPixmap, RetainPermanent, XA_PIXMAP, PropModeReplace = 2, 1, 20, 0

img = Image.open(sys.argv[1]).convert('RGB')
w, h = img.size
data = img.convert('RGBX').tobytes('raw', 'BGRX')  # 32 bits per pixel, as a 24-bit visual stores it

d = x.XOpenDisplay(None)
if not d:
    sys.exit('setroot: cannot open the display')
s = x.XDefaultScreen(d)
root = x.XRootWindow(d, s)
depth = x.XDefaultDepth(d, s)
pixmap = x.XCreatePixmap(d, root, w, h, depth)
gc = x.XCreateGC(d, pixmap, 0, None)
buf = ctypes.create_string_buffer(data, len(data))
image = x.XCreateImage(d, x.XDefaultVisual(d, s), depth, ZPixmap, 0, ctypes.cast(buf, ctypes.c_char_p), w, h, 32, w * 4)
x.XPutImage(d, pixmap, gc, image, 0, 0, 0, 0, w, h)
x.XSetWindowBackgroundPixmap(d, root, pixmap)
x.XClearWindow(d, root)
pid = ctypes.c_ulong(pixmap)
for name in (b'_XROOTPMAP_ID', b'ESETROOT_PMAP_ID'):
    x.XChangeProperty(d, root, x.XInternAtom(d, name, 0), XA_PIXMAP, 32, PropModeReplace, ctypes.byref(pid), 1)
# Keep the pixmap after this client exits. (The XImage still points at Python's buffer: it is not
# destroyed, so Xlib never frees memory it does not own.)
x.XSetCloseDownMode(d, RetainPermanent)
x.XFlush(d)
x.XCloseDisplay(d)
