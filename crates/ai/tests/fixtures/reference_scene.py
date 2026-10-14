import math

import manimgx as m


def wave(x: float) -> float:
    return 1.5 * math.sin(x)


class LessonScene(m.Scene):
    """Exercises every construct the prompt's ManimGX reference lists."""

    def construct(self) -> None:
        title = m.Text("A tour of the reference", font_size=48, t2c={"tour": m.YELLOW})
        title.to_edge(m.UP)
        self.say("This scene uses [every construct] in the reference.", m.Write(title))

        axes = m.Axes(x_range=[0, 6, 1], y_range=[-2, 2, 1], x_length=6, y_length=3.6)
        axes.move_to([-3.2, -0.6, 0])
        axes.add_coordinates()
        names = axes.get_axis_labels("x", "y")
        graph = axes.plot(wave, x_range=[0, 6], color=m.BLUE)
        label = axes.get_graph_label(graph, r"y = \sin x", x_val=1.6, direction=m.UP)
        area = axes.get_area(graph, x_range=[0, math.pi], opacity=0.4)
        self.say(
            "Here are [axes with numbers], [a sine wave] and [the area under its first hump].",
            m.AnimationGroup(m.Create(axes), m.Write(names)),
            m.AnimationGroup(m.Create(graph), m.Write(label)),
            m.FadeIn(area),
        )

        dot = m.Dot(axes.c2p(0, 0), color=m.YELLOW)
        self.add(dot)
        self.say("A dot [rides along the curve].", m.MoveAlongPath(dot, graph))
        self.say(
            "We can [flash it], [wiggle the label] and [circle the area].",
            m.Flash(dot),
            m.Wiggle(label),
            m.Circumscribe(area),
        )
        self.play(m.FadeOut(*self.mobjects), run_time=0.8)

        line = m.NumberLine(x_range=[-2, 2, 1], length=8, include_numbers=True)
        line.move_to([0, -2.6, 0])
        value = m.DecimalNumber(3.14, num_decimal_places=2, font_size=40).to_corner(m.UL)
        mixed = m.Tex(r"The area is $\pi r^2$.", font_size=40).to_corner(m.UR)
        self.say(
            "[A number line], [a decimal number] and [words mixed with math].",
            m.Create(line),
            m.FadeIn(value, shift=m.UP),
            m.Write(mixed),
        )

        box = m.RoundedRectangle(corner_radius=0.2, width=3, height=1.6, color=m.TEAL)
        box.move_to([-4, 0.6, 0])
        arc = m.Arc(radius=0.8, start_angle=0, angle=m.PI / 2, color=m.ORANGE)
        arc.move_to([-0.8, 0.6, 0])
        dashed = m.DashedLine([1.2, 0.2, 0], [3.2, 0.2, 0], color=m.GREY_B)
        double = m.DoubleArrow([1.2, 1.0, 0], [3.2, 1.0, 0], buff=0)
        arrow = m.Arrow([4.2, 0.2, 0], [6.0, 1.0, 0], buff=0, color=m.PINK)
        self.say(
            "Shapes: [a rounded box], [an arc], [a dashed line], [a double arrow] and [an arrow].",
            m.Create(box),
            m.Create(arc),
            m.Create(dashed),
            m.GrowFromCenter(double),
            m.GrowArrow(arrow),
        )

        brace = m.Brace(dashed, m.DOWN)
        brace_label = brace.get_tex(r"n")
        circle = m.Circle(radius=0.5, color=m.RED).move_to([-4, 0.6, 0])
        self.say(
            "[A brace with a label], then [a copy of the box becomes a circle].",
            m.AnimationGroup(m.GrowFromCenter(brace), m.Write(brace_label)),
            m.TransformFromCopy(box, circle),
        )
        self.say(
            "Animations can [run in turn], or [one after another].",
            m.LaggedStart(
                m.Indicate(box), m.Indicate(arc), m.Indicate(arrow), lag_ratio=0.3
            ),
            m.Succession(
                m.Rotate(arc, m.PI / 2), arc.animate.shift(0.3 * m.DOWN).set_color(m.RED)
            ),
        )
        self.say(
            "And they can [undo their drawing].",
            m.Uncreate(m.VGroup(box, circle, arc, dashed, double, arrow, brace, brace_label)),
        )
        self.play(m.FadeOut(*self.mobjects), run_time=0.8)

        before = m.MathTex("a^2", "+", "b^2", "=", "c^2", font_size=64)
        after = m.MathTex("c", "=", r"\sqrt{a^2 + b^2}", font_size=64)
        stack = m.VGroup(
            m.MathTex(r"\int_0^1 x^2 \, dx = \frac{1}{3}", font_size=40),
            m.MathTex(r"\lim_{h \to 0} \frac{\sin h}{h} = 1", font_size=40),
            m.MathTex(r"\sum_{k=1}^{n} k = \frac{n(n+1)}{2}", font_size=40),
        ).arrange(m.DOWN, buff=0.4)
        stack.move_to([3.5, 0, 0])
        before.move_to([-3.2, 0.5, 0])
        after.move_to([-3.2, 0.5, 0])
        before[2].set_color(m.YELLOW)
        self.say(
            "Formulas [write themselves], [rearrange], and [stack neatly].",
            m.Write(before),
            m.TransformMatchingTex(before, after),
            m.FadeIn(stack, shift=m.LEFT),
        )
        self.play(m.FadeOut(*self.mobjects), run_time=0.8)
        self.wait(1)
