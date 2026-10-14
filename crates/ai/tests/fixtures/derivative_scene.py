import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        title = m.Text("What a derivative measures", font_size=48).to_edge(m.UP)
        self.say(
            "Today we ask one question: [how fast is a function changing] "
            "at a single point?",
            m.Write(title),
        )

        axes = m.Axes(
            x_range=[-1, 3, 1], y_range=[-1, 5, 1], x_length=6.5, y_length=5
        ).move_to([-3.2, -0.6, 0])
        graph = axes.plot(lambda x: x**2 / 2, x_range=[-1, 3], color=m.BLUE)
        label = m.MathTex(r"f(x) = \frac{x^2}{2}", font_size=40).move_to([3.6, 2.2, 0])
        self.say(
            "Here are [some axes], and on them "
            "[the curve f of x equals x squared over two].",
            m.Create(axes),
            m.AnimationGroup(m.Create(graph), m.Write(label)),
        )

        p = m.Dot(axes.c2p(1, 0.5), color=m.YELLOW)
        q = m.Dot(axes.c2p(2.5, 3.125), color=m.YELLOW)
        secant = m.Line(axes.c2p(0, -0.25), axes.c2p(3, 4.25), color=m.ORANGE)
        self.say(
            "Pick [a point on the curve], and [a second point further along]. "
            "[The line through both] rises a certain amount for every step to the right.",
            m.GrowFromCenter(p),
            m.GrowFromCenter(q),
            m.Create(secant),
        )

        quotient = m.MathTex(r"\frac{f(x+h) - f(x)}{h}", font_size=40).move_to(
            [3.6, 0.9, 0]
        )
        self.say(
            "That rise over run is [the difference quotient], "
            "the slope of the line between the two points.",
            m.Write(quotient),
        )

        q2 = m.Dot(axes.c2p(1.2, 0.72), color=m.YELLOW)
        tangent = m.Line(axes.c2p(0, -0.5), axes.c2p(3, 2.5), color=m.GREEN)
        self.say(
            "Now slide the second point [toward the first]. "
            "As the gap h shrinks, [the line settles into the tangent].",
            m.Transform(q, q2),
            m.ReplacementTransform(secant, tangent),
        )

        limit = m.MathTex(
            r"f'(x) = \lim_{h \to 0} \frac{f(x+h) - f(x)}{h}", font_size=36
        ).move_to([3.6, -0.6, 0])
        box = m.SurroundingRectangle(limit, color=m.YELLOW, buff=0.15)
        self.say(
            "The slope of that tangent is [the derivative]: the limit of the "
            "difference quotient as h goes to zero. [Remember this formula].",
            m.Write(limit),
            m.Create(box),
        )

        answer = m.MathTex(r"f'(1) = 1", font_size=40).move_to([3.6, -2.2, 0])
        self.say(
            "For our curve, at x equals one, [the slope comes out to exactly one]. "
            "The derivative is that instant rate of change.",
            m.Write(answer),
        )
        self.say("Keep that picture in mind: a derivative is a slope.")
        self.say("[Let's clear the board.]", m.FadeOut(*self.mobjects))
        self.wait(1)
