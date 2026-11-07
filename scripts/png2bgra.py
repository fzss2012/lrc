
from PIL import Image
import numpy as np
img = Image.open(r"D:\lrc\captures\office-shot.png").convert("RGB")
w, h = img.size
print("png", img.size)
arr = np.asarray(img)
bgra = np.dstack([arr[:,:,2], arr[:,:,1], arr[:,:,0], np.full((h,w),255,np.uint8)])
raw = bgra.tobytes()
open(r"D:\lrc\captures\office-frame.bgra","wb").write(raw)
nz = int((np.frombuffer(raw, dtype=np.uint8)[::997] != 0).sum())
print("wrote office-frame.bgra", len(raw), "nonzero_samples", nz, "/", len(raw)//997+1)
