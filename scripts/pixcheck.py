
from PIL import Image
import numpy as np
src = Image.open(r"D:\lrc\captures\pix-source.png").convert("RGB")
cli = Image.open(r"D:\lrc\captures\pix-client-screen.png").convert("RGB")
print("source", src.size, "client area", cli.size)
w, h = cli.size
src_r = src.resize((w, h), Image.LANCZOS)
a = np.asarray(src_r, dtype=np.int16); b = np.asarray(cli, dtype=np.int16)
d = np.abs(a-b)
print("mean abs diff = %.2f  p99 = %.1f" % (d.mean(), np.percentile(d, 99)))
diff = d.sum(axis=2)
mask = diff > 60
print("changed>60 ratio: %.4f" % float(mask.mean()))
if mask.any():
    ys, xs = np.nonzero(mask)
    print("bbox x[%d..%d] y[%d..%d]" % (xs.min(), xs.max(), ys.min(), ys.max()))
Image.fromarray(np.clip(diff,0,255).astype(np.uint8)).save(r"D:\lrc\captures\pix-diff.png")
