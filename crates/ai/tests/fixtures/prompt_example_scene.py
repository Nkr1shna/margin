import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        title = m.Text("The Pythagorean theorem", font_size=48).to_edge(m.UP)
        self.add(title)
        self.say(
            "Let's look at [the Pythagorean theorem].",
            m.Indicate(title),
        )
        tri = m.Polygon(
            [-5.5, -2.5, 0],
            [-2.5, -2.5, 0],
            [-5.5, 0.5, 0],
            color=m.WHITE,
            fill_opacity=0.3,
        )
        formula = m.MathTex("a^2", "+", "b^2", "=", "c^2", font_size=72).move_to(
            [2.5, 1.2, 0]
        )
        self.say(
            "For [a right triangle], [a squared plus b squared equals c squared].",
            m.Create(tri),
            m.Write(formula),
        )
        self.say(
            "Each term is a square's area: [the first leg], [the second], "
            "and [the hypotenuse].",
            m.Indicate(formula[0]),
            m.Indicate(formula[2]),
            m.Indicate(formula[4]),
        )
        self.say("So the two small squares together exactly fill the big one.")
        self.say(
            "[Clear the board], and keep that one equation in mind.",
            m.FadeOut(*self.mobjects),
        )

        # The poster shot (the thumbnail): title and key formula together, then held.
        poster_title = m.Text("The Pythagorean theorem", font_size=56).to_edge(m.UP)
        poster_formula = m.MathTex("a^2 + b^2 = c^2", font_size=96).move_to([0, -0.4, 0])
        self.play(m.FadeIn(poster_title), m.Write(poster_formula))
        self.wait(2)
