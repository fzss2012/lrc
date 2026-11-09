/* x11-pattern.c —— 在 X11 屏幕上画确定性测试图案（供 verify-linux.ps1 做像素/帧率验证）
 *
 * 编译：gcc -O2 -o x11-pattern x11-pattern.c -lX11
 * 用法：
 *   x11-pattern --static [--seconds 30]     画 8x8 已知颜色格子，静止不动
 *   x11-pattern --anim   [--seconds 30]     同样格子 + 一个来回移动的白色方块（产生运动）
 *   可选：--cell N（格子像素，默认 = 屏宽/8）
 *
 * 静止模式用来做像素比对：格子 (i,j) 的颜色由固定公式给出，验证脚本可以复算：
 *   r = (i*37 + j*59) & 0xFF
 *   g = (i*17 + j*23) & 0xFF
 *   b = (i*71 + j*11) & 0xFF
 * 动画模式用来验证 fps/脏块：白方块每帧移动 8px，碰到边缘反弹。
 */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>

static int cell_rgb(int i, int j, int c) {
    if (c == 0) return (i * 37 + j * 59) & 0xFF;
    if (c == 1) return (i * 17 + j * 23) & 0xFF;
    return (i * 71 + j * 11) & 0xFF;
}

static unsigned long alloc_rgb(Display *dpy, Colormap cmap, int r, int g, int b) {
    XColor c;
    c.red = (unsigned short)(r * 257);
    c.green = (unsigned short)(g * 257);
    c.blue = (unsigned short)(b * 257);
    c.flags = DoRed | DoGreen | DoBlue;
    if (!XAllocColor(dpy, cmap, &c)) return BlackPixel(dpy, DefaultScreen(dpy));
    return c.pixel;
}

int main(int argc, char **argv) {
    int anim = 0, seconds = 30, cell = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--anim")) anim = 1;
        else if (!strcmp(argv[i], "--static")) anim = 0;
        else if (!strcmp(argv[i], "--seconds") && i + 1 < argc) seconds = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--cell") && i + 1 < argc) cell = atoi(argv[++i]);
    }
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) { fprintf(stderr, "x11-pattern: 打不开 DISPLAY\n"); return 2; }
    int scr = DefaultScreen(dpy);
    Window root = RootWindow(dpy, scr);
    int W = DisplayWidth(dpy, scr), H = DisplayHeight(dpy, scr);
    if (cell <= 0) cell = W / 8 > 0 ? W / 8 : 1;

    XSetWindowAttributes swa;
    swa.override_redirect = True;
    swa.background_pixel = BlackPixel(dpy, scr);
    Window win = XCreateWindow(dpy, root, 0, 0, (unsigned)W, (unsigned)H, 0,
                               CopyFromParent, InputOutput, CopyFromParent,
                               CWOverrideRedirect | CWBackPixel, &swa);
    XMapRaised(dpy, win);
    XStoreName(dpy, win, "lrc-test-pattern");

    GC gc = XCreateGC(dpy, win, 0, NULL);
    Colormap cmap = DefaultColormap(dpy, scr);
    int cols = (W + cell - 1) / cell, rows = (H + cell - 1) / cell;
    if (cols > 8) cols = 8;
    if (rows > 8) rows = 8;
    printf("size=%dx%d cell=%d cols=%d rows=%d anim=%d\n", W, H, cell, cols, rows, anim);
    fflush(stdout);

    /* 预分配格子颜色 */
    unsigned long *pix = (unsigned long *)malloc(sizeof(unsigned long) * (size_t)(cols * rows));
    if (!pix) return 3;
    for (int j = 0; j < rows; j++)
        for (int i = 0; i < cols; i++)
            pix[j * cols + i] = alloc_rgb(dpy, cmap, cell_rgb(i, j, 0), cell_rgb(i, j, 1), cell_rgb(i, j, 2));

    time_t t0 = time(NULL);
    int bx = 0, by = 0, dx = 8, dy = 6, sq = W / 12 > 40 ? W / 12 : 40;
    while ((int)(time(NULL) - t0) < seconds) {
        for (int j = 0; j < rows; j++) {
            for (int i = 0; i < cols; i++) {
                XSetForeground(dpy, gc, pix[j * cols + i]);
                XFillRectangle(dpy, win, gc, i * cell, j * cell, (unsigned)cell, (unsigned)cell);
            }
        }
        if (anim) {
            bx += dx; by += dy;
            if (bx < 0) { bx = 0; dx = -dx; }
            if (by < 0) { by = 0; dy = -dy; }
            if (bx + sq > W) { bx = W - sq; dx = -dx; }
            if (by + sq > H) { by = H - sq; dy = -dy; }
            XSetForeground(dpy, gc, alloc_rgb(dpy, cmap, 255, 255, 255));
            XFillRectangle(dpy, win, gc, bx, by, (unsigned)sq, (unsigned)sq);
            XFlush(dpy);
            usleep(33000); /* ~30fps */
        } else {
            XFlush(dpy);
            usleep(200000);
        }
    }
    XFreeGC(dpy, gc);
    XDestroyWindow(dpy, win);
    XCloseDisplay(dpy);
    return 0;
}
