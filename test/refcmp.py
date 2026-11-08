"""Side-by-side and error metrics for --render-ref output: <dir>/<beat>.png (ChroMapper) vs ours-<beat>.png.
usage: python3 -I test/refcmp.py <capture dir>..."""
import sys, glob, os
from PIL import Image, ImageChops, ImageStat
for d in sys.argv[1:]:
    for ours in sorted(g for g in glob.glob(os.path.join(d, 'ours-*.png')) if 'ours-fog-' not in g and 'ours-refl-' not in g):
        beat = os.path.basename(ours)[5:-4]
        cm = Image.open(os.path.join(d, beat + '.png')).convert('RGB')
        us = Image.open(ours).convert('RGB').resize(cm.size)
        diff = ImageChops.difference(cm, us)
        mae = sum(ImageStat.Stat(diff).mean) / 3
        side = Image.new('RGB', (cm.width, cm.height * 3))
        side.paste(cm, (0, 0)); side.paste(us, (0, cm.height))
        side.paste(diff.point(lambda v: min(255, v * 3)), (0, cm.height * 2))
        side.save(os.path.join(d, 'cmp-' + beat + '.png'))
        print(f'{d} {beat}: mean abs error {mae:.2f}/255')
