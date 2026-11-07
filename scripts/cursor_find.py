
from PIL import Image
import numpy as np

A = np.asarray(Image.open(r"D:\lrc\captures\cur-A.png").convert("L"), dtype=np.float32)
B = np.asarray(Image.open(r"D:\lrc\captures\cur-B.png").convert("L"), dtype=np.float32)
C = np.asarray(Image.open(r"D:\lrc\captures\cur-C.png").convert("L"), dtype=np.float32)

# 从 A 里光标所在处取模板（目测在 (235,230) 附近）
tx, ty, ts = 228, 228, 26
tpl = A[ty:ty+ts, tx:tx+ts]
print("template brightness mean", tpl.mean(), "max", tpl.max())

def match(img, tpl):
    th, tw = tpl.shape
    h, w = img.shape
    tpl0 = tpl - tpl.mean()
    tpln = np.sqrt((tpl0**2).sum())
    best = (-2.0, -1, -1)
    # 只在硬件里搜索（步长 1）
    for y in range(0, h-th):
        row = img[y:y+th]
        for x in range(0, w-tw):
            p = row[:, x:x+tw]
            p0 = p - p.mean()
            d = np.sqrt((p0**2).sum())
            if d < 1e-3:
                continue
            s = float((p0*tpl0).sum()/(d*tpln))
            if s > best[0]:
                best = (s, x, y)
    return best

for name, img in (("A", A), ("B", B), ("C", C)):
    s, x, y = match(img, tpl)
    print(f"{name}: best NCC={s:.3f} at ({x},{y})")
