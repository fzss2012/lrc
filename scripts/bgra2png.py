
import struct, sys
src = r"D:\lrc\captures\frame0.bgra"
data = open(src, "rb").read()
w, h = 1920, 1080
print("bytes:", len(data), "expected:", w*h*4, "match:", len(data) == w*h*4)
try:
    from PIL import Image
    img = Image.frombytes("RGBA", (w, h), data, "raw", "BGRA")
    img = img.convert("RGB")
    img.resize((960, 540), Image.LANCZOS).save(r"D:\lrc\captures\frame0.png")
    print("saved D:\\lrc\\captures\\frame0.png")
    # 粗略色彩统计，判断是否明显通道错乱
    px = img.resize((64, 36))
    colors = px.getcolors(64*36)
    colors.sort(reverse=True)
    print("top colors:", [(c, rgb) for c, rgb in colors[:5]])
except ImportError as e:
    print("no PIL:", e)
