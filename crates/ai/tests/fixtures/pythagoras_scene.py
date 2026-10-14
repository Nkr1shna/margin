import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        title = m.Text("The Pythagorean theorem", font_size=48).to_edge(m.UP)
        self.say(
            "Let's look at [the Pythagorean theorem].",
            m.FadeIn(title, shift=m.DOWN),
        )

        tri = m.Polygon(
            [-5.5, -2.5, 0],
            [-2.5, -2.5, 0],
            [-5.5, 0.5, 0],
            color=m.WHITE,
            fill_opacity=0.3,
        )
        corner = m.Square(side_length=0.3, color=m.WHITE).move_to([-5.35, -2.35, 0])
        la = m.MathTex("a", font_size=40).move_to([-4.0, -2.9, 0])
        lb = m.MathTex("b", font_size=40).move_to([-5.9, -1.0, 0])
        lc = m.MathTex("c", font_size=40).move_to([-3.7, -0.7, 0])
        self.say(
            "Start with [a right triangle]. Call its legs [a and b], "
            "and its longest side, the hypotenuse, [c].",
            m.AnimationGroup(m.Create(tri), m.Create(corner)),
            m.AnimationGroup(m.Write(la), m.Write(lb)),
            m.Write(lc),
        )

        formula = m.MathTex("a^2", "+", "b^2", "=", "c^2", font_size=72).move_to(
            [2.5, 1.2, 0]
        )
        self.say(
            "The theorem says that [a squared plus b squared equals c squared].",
            m.Write(formula),
        )

        self.say(
            "Each term is the area of a square: [one on the first leg], "
            "[one on the second], and [one on the hypotenuse].",
            m.Indicate(formula[0]),
            m.Indicate(formula[2]),
            m.Indicate(formula[4]),
        )

        example = m.MathTex(r"3^2 + 4^2 = 9 + 16 = 25 = 5^2", font_size=44).move_to(
            [2.5, -0.6, 0]
        )
        self.say(
            "Try it with legs three and four: [nine plus sixteen is twenty five], "
            "which is five squared.",
            m.Write(example),
        )

        solved = m.MathTex("c", "=", r"\sqrt{a^2 + b^2}", font_size=72).move_to(
            [2.5, 1.2, 0]
        )
        self.say(
            "So if you know both legs, [you can always find the hypotenuse].",
            m.TransformMatchingTex(formula, solved),
        )

        self.say(
            "[Clear the board], and keep that one equation in mind.",
            m.FadeOut(m.VGroup(title, tri, corner, la, lb, lc, example, solved)),
        )

        # The poster shot: the title, the triangle and its theorem, held to the end.
        poster_title = m.Text("The Pythagorean theorem", font_size=56).to_edge(m.UP)
        poster_tri = m.Polygon(
            [-5.5, -2.8, 0],
            [-2.0, -2.8, 0],
            [-5.5, 0.7, 0],
            color=m.BLUE,
            fill_opacity=0.4,
        )
        poster_formula = m.MathTex("a^2 + b^2 = c^2", font_size=80).move_to(
            [2.3, -1.0, 0]
        )
        self.play(m.FadeIn(poster_title), m.Create(poster_tri), m.Write(poster_formula))
        self.wait(2)
