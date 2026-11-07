
from PIL import Image
import numpy as np
A = Image.open(r"D:\lrc\captures\cur-A.png").convert("RGB")
B = Image.open(r"D:\lrc\captures\cur-B.png").convert("RGB")
C = Image.open(r"D:\lrc\captures\cur-C.png").convert("RGB")
print("sizes", A.size, B.size, C.size)
exA = (263, 200); exB = (790, 467)
def crop(img, cx, cy, r=60):
    x0 = max(0, cx-r); y0 = max(0, cy-r)
    x1 = min(img.width, cx+r); y1 = min(img.height, cy+r)
    return img.crop((x0, y0, x1, y1))
# 找差异最集中的小区域：用 diff A/B，先做块级聚合
a = np.asarray(A, dtype=np.int16); b = np.asarray(B, dtype=np.int16)
d = np.abs(a-b).sum(axis=2)
h, w = d.shape
bs = 8
hb, wb = h//bs, w//bs
blocks = d[:hb*bs, :wb*bs].reshape(hb, bs, wb, bs).sum(axis=(1,3))
idx = np.argsort(blocks.ravel())[-6:][::-1]
print("A/B 差异最集中的 8x8 块（按差异总和）：")
for i in idx:
    by, bx = divmod(int(i), wb)
    print(f"  block@({bx*bs},{by*bs}) diff={blocks[by,bx]}")
crops = [crop(A, *exA), crop(B, *exB)]
# 放大 3x 并排
scaled = [c.resize((c.width*3, c.height*3), Image.NEAREST) for c in crops]
out = Image.new("RGB", (sum(s.width for s in scaled)+10, max(s.height for s in scaled)), (40,40,40))
x = 0
for s in scaled:
    out.paste(s, (x, 0)); x += s.width + 10
out.save(r"D:\lrc\captures\cursor-crops.png")
print("saved cursor-crops.png", out.size)
