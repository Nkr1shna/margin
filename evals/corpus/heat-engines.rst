============
Heat engines
============

Written for the Margin evaluation corpus. Released under CC0.

A heat engine takes heat from a hot reservoir, turns part of it into work, and rejects the rest to a cold reservoir. Steam turbines, car engines and jet engines are all heat engines. Thermodynamics puts a hard ceiling on how much of the heat can become work, and the ceiling depends only on the temperatures of the two reservoirs.

Efficiency and the first law
============================

Over one full cycle the working fluid returns to its starting state, so its internal energy does not change. The first law then says the net work W equals the heat taken in, Q_h, minus the heat rejected, Q_c. The thermal efficiency is the fraction of the input heat that becomes work:

.. math::

   \eta = W / Q_h = 1 - Q_c / Q_h

An engine that rejected no heat would have an efficiency of 1. The second law says no such engine exists.

The second law
==============

Two classical statements of the second law turn out to be equivalent.

Kelvin-Planck statement
   No cyclic device can take heat from a single reservoir and convert all of it into work.

Clausius statement
   No cyclic device can have as its only effect the transfer of heat from a colder body to a hotter one.

If a device violated the Clausius statement, it could be paired with an ordinary engine to build a machine that violated the Kelvin-Planck statement, and the reverse construction also works; that is why the two statements stand or fall together.

The Carnot cycle
================

The Carnot cycle is the most efficient cycle possible between two reservoirs. It is reversible and consists of four steps:

1. Isothermal expansion at the hot temperature T_h, absorbing heat Q_h.
2. Adiabatic expansion, during which the gas cools from T_h to T_c.
3. Isothermal compression at the cold temperature T_c, rejecting heat Q_c.
4. Adiabatic compression back to the starting state, heating the gas from T_c to T_h.

For a reversible cycle the heats are in the same ratio as the absolute temperatures, Q_c / Q_h = T_c / T_h, so the Carnot efficiency is

.. math::

   \eta_{Carnot} = 1 - T_c / T_h

with both temperatures in kelvin. An engine running between steam at 500 K and cooling water at 300 K can therefore never exceed 40 percent efficiency, however well it is built. Real engines fall short of the Carnot limit because of friction, heat leaking across finite temperature differences, and other irreversibilities.

Carnot's theorem states that no engine operating between two reservoirs can be more efficient than a reversible engine operating between the same reservoirs, and that all reversible engines between the same reservoirs have the same efficiency, whatever their working substance.

Entropy
=======

The Carnot result defines a state function. For any reversible process the change in entropy is dS = dQ_rev / T. Around a reversible cycle the entropy changes sum to zero, and for an isolated system the total entropy never decreases. In an irreversible engine, the entropy rejected to the cold reservoir exceeds the entropy taken from the hot one; the excess is entropy generated inside the engine, and it is exactly what costs efficiency.

When a heat Q flows directly from a reservoir at T_h to one at T_c with no work done, the entropy of the universe rises by Q/T_c − Q/T_h, which is positive whenever T_h is greater than T_c.

The Otto cycle
==============

The Otto cycle is the idealised model of a spark-ignition petrol engine: adiabatic compression, heat added at constant volume (combustion), adiabatic expansion (the power stroke), and heat rejected at constant volume (exhaust). For an ideal gas with heat-capacity ratio gamma and compression ratio r, the efficiency is

.. math::

   \eta_{Otto} = 1 - r^{1 - \gamma}

With r = 10 and gamma = 1.4 the ideal efficiency is about 60 percent. Higher compression ratios raise efficiency, but compressing the fuel-air mixture too far makes it ignite before the spark, which is knock; that is why petrol engines stay near compression ratios of 10 to 12 while diesel engines, which compress air alone and inject fuel at the top of the stroke, run at 15 to 20.

Refrigerators and heat pumps
============================

Running a heat engine backwards moves heat from cold to hot at the cost of work. A refrigerator is judged by its coefficient of performance, the heat removed from the cold space per unit of work: COP_R = Q_c / W. The Carnot limit for a refrigerator is T_c / (T_h − T_c). A heat pump used for heating is judged by the heat delivered to the warm space instead, COP_HP = Q_h / W, whose Carnot limit is T_h / (T_h − T_c). Because COP_HP = COP_R + 1, a heat pump always delivers more heat than the electrical work it consumes, which is why it beats a resistance heater.
