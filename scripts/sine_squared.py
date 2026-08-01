# Per-sample transform: square the demo sine wave.
#
# Reads one input channel, returns one output value per input sample
# (a length-preserving map), keeping each sample's original timestamp.
# This is the simplest shape a script can take: one input, one output,
# a bare (ts, vals) return.

import numba

INPUTS = ["load/ch0"]
OUTPUTS = [{"name": "scripts.{in0.stem}_squared", "type": "float", "unit": ""}]


@numba.njit
def compute(ts, vals):
    t = ts[0]        # timestamps of load/ch0 (int64 ns)
    x = vals[0]      # its values over the last window_s seconds (float64)
    return (t, x * x)
