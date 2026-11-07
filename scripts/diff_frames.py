
from PIL import Image, ImageChops
import numpy as np
a = Image.open(r"D:\lrc\captures\pat-a.png").convert("RGB")
b = Image.open(r"D:\lrc\captures\pat-b.png").convert("RGB")
c = Image.open(r"D:\lrc\captures\pat-c.png").convert("RGB")
for name, x, y in (("a-b", a, b), ("b-c", b, c)):
    na, nb = np.asarray(x, dtype=np.int16), np.asarray(y, dtype=np.int16)
    d = np.abs(na - nb).sum(axis=2)
    changed = d > 12
    n = int(changed.sum())
    print(f"{name}: changed_px={n} ({n/changed.size*100:.3f}%)")
    if n:
        ys, xs = np.nonzero(changed)
        print(f"   bbox x[{xs.min()}..{xs.max()}] y[{ys.min()}..{ys.max()}]")
        # 按行聚合看有几块
        rows = np.nonzero(changed.any(axis=1))[0]
        segs = []
        start = rows[0]; prev = rows[0]
        for r0 in rows[1:]:
            if r0 - prev > 4:
                segs.append((start, prev)); start = r0
            prev = r0
        segs.append((start, prev))
        print(f"   row_segments={segs[:10]}")
