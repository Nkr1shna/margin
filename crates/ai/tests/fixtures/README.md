# ManimGX scene fixtures

These scenes follow exactly the rules `write_scene` gives the model (one `LessonScene`,
`import manimgx as m` plus optionally `math`, narration only through `self.say` with one
`[bracketed]` span per animation, no `voice`, inside x in [-7, 7] and y in [-4, 4]).
They prove that those rules produce scenes ManimGX 0.1.1 renders.

- `prompt_example_scene.py` is included verbatim in the prompt (`prompts::EXAMPLE_SCENE`).
- `reference_scene.py` uses every construct the prompt's ManimGX reference lists.
- `derivative_scene.py` and `pythagoras_scene.py` are full lesson-style scenes.

They have no voice. To render one, append a stand-in voice that returns silence, as the
renderer does with its own voice (a scene's `voice` is any function from text to
`m.Speech`; ManimGX caches each line under `voice/` beside the file):

```python
LessonScene.voice = staticmethod(lambda text: m.Speech("/path/to/silence.wav", text=text))
```

then:

```sh
manimgx inspect scene.py                       # exit 0: no layout problems
manimgx render -r 854x480 --fps 24 -o out/scene.mp4 scene.py
```

`render` needs the output directory to exist. All four rendered with exit 0 and "layout:
no problems" on macOS arm64 with manimgx 0.1.1 (Metal), using a 6-second silent 48 kHz
mono WAV.
