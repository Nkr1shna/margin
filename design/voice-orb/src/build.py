"""Builds index.html and the option pages from the shared core and each page's own part.
Run: python3 -I src/build.py"""
import pathlib
SRC = pathlib.Path(__file__).resolve().parent
OUT = SRC.parent
base = (SRC / 'base.html').read_text()
core_a, core_b, sound = (SRC / 'core-a.js').read_text() + '\n' + (SRC / 'glass.glsl.js').read_text(), (SRC / 'core-b.js').read_text(), (SRC / 'sound.js').read_text()
DOC = '<!doctype html>\n<html lang="en">\n<head>\n<meta charset="utf-8">\n<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">\n</head>\n<body>\n'
PAGES = [
    # file, page source, title, name, accessible description
    ('index.html', 'page-pearl.js', 'Pearl Orb', 'Pearl', 'A liquid-glass pearl that breathes, listens, thinks and speaks.'),
    ('option-droplet.html', 'page-droplet.js', 'Droplet Orb', 'Droplet', 'A ruby glass droplet that splits into beads and gathers them back.'),
    ('option-medusa.html', 'page-medusa.js', 'Medusa Orb', 'Medusa', 'A translucent bell with trailing filaments that pulses as it speaks.'),
    ('option-silk.html', 'page-silk.js', 'Silk Orb', 'Silk', 'Fine silk threads that drift and gather into a sphere when it speaks.'),
]
for out, src, title, name, label in PAGES:
    if not (SRC / src).exists():
        continue
    # the published main page is wrapped in a document by the host, so index.html has no skeleton
    head, tail = ('', '') if out == 'index.html' else (DOC, '</body>\n</html>\n')
    html = base
    for k, v in {'HEAD': head, 'TAIL': tail, 'TITLE': title, 'NAME': name, 'LABEL': label, 'CORE_A': core_a, 'SOUND': sound, 'PAGE': (SRC / src).read_text(), 'CORE_B': core_b}.items():
        html = html.replace('{{' + k + '}}', v)
    assert '{{' not in html.replace('${{', ''), out
    (OUT / out).write_text(html)
    print(out, len(html))
