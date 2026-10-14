# TCP congestion control

Written for the Margin evaluation corpus. Released under CC0.

## Why senders must slow down

A TCP sender limits how much unacknowledged data it has in flight with two windows: the receiver's advertised window, which protects the receiver's buffer, and the congestion window, cwnd, which protects the network. The sender may have at most the smaller of the two outstanding. Without a congestion window, every sender would transmit as fast as the receiver allowed; queues in routers would overflow, dropped packets would be retransmitted, and the retransmissions would add to the overload. That collapse happened on the early Internet in 1986, when throughput between two sites fell by a factor of about a thousand, and Van Jacobson's congestion-control algorithms of 1988 were the response.

TCP has no direct signal from the routers in the classic design. It infers congestion from loss: a retransmission timeout, or several duplicate acknowledgements, means a packet was probably dropped at a full queue.

## Slow start

A new connection does not know the capacity of its path, so it starts with a small congestion window, a few segments in modern stacks, and grows it quickly. In slow start the sender increases cwnd by one segment for every acknowledgement it receives. Since each round trip delivers a full window of acknowledgements, cwnd doubles every round-trip time: growth is exponential despite the name. Slow start ends when cwnd reaches the slow-start threshold, ssthresh, or when a loss is detected.

## Congestion avoidance: additive increase, multiplicative decrease

Above ssthresh the sender is in congestion avoidance and grows cwnd by about one segment per round-trip time, a linear probe for spare capacity. When it detects a loss, it cuts its rate. This rule, additive increase and multiplicative decrease (AIMD), is what makes competing flows converge toward a fair share: two flows that increase by the same amount and decrease by the same fraction move closer to equal rates with each cycle, whatever rates they started from.

## Fast retransmit and fast recovery

Waiting for a retransmission timeout wastes time, because timers are set conservatively. A receiver that gets a segment out of order immediately repeats its last acknowledgement. When the sender sees three duplicate acknowledgements, it concludes that the segment after the acknowledged data was lost while later segments got through, and it retransmits that segment at once without waiting for the timer. This is fast retransmit.

In TCP Reno, fast retransmit is followed by fast recovery: the sender sets ssthresh to half of the current window, sets cwnd to ssthresh, and continues in congestion avoidance rather than falling back to slow start. The duplicate acknowledgements show that data is still flowing, so a severe reaction is unnecessary. A retransmission timeout, by contrast, is treated as a sign of heavy congestion: ssthresh is set to half the current window and cwnd drops to one segment, restarting slow start.

Plotting cwnd over time for a long Reno transfer gives the characteristic sawtooth: linear climbs, each ending in a halving when the queue overflows.

## CUBIC and BBR

On fast, long-distance paths, Reno's growth of one segment per round trip takes too long to refill a large window after a loss. CUBIC, the default in Linux for many years, grows the window as a cubic function of the time since the last loss: fast when far from the window size at which the loss occurred, slow and careful near it, and fast again beyond it. Because its growth depends on elapsed time rather than on the arrival of acknowledgements, flows with different round-trip times share a link more fairly than under Reno.

BBR takes a different approach altogether. Instead of waiting for loss, it measures the bottleneck bandwidth and the minimum round-trip time and paces packets to match their product, the bandwidth-delay product. A loss-based algorithm keeps buffers full until they overflow; BBR aims to keep the pipe full but the queue nearly empty.

## Bufferbloat

Large buffers in routers and home gateways make loss-based congestion control worse, not better. A loss-based sender keeps increasing its window until a buffer overflows, so an oversized buffer fills up and stays full, adding hundreds of milliseconds of queueing delay to every packet that shares it. This is bufferbloat. Active queue management schemes such as CoDel drop or mark packets when the delay through the queue, rather than its length, stays above a target, signalling senders to slow down before the queue grows long.
