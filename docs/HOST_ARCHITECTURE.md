# Live host architecture

The Linux live-process boundary wraps the pure engine without moving package or
render ownership into the host. Software validation is not connected JACK,
ALSA, latency, polyphony, or audible evidence.

## Boundaries

`shr-sampler-core` continues to own package validation, WAV decoding, prepared
instrument data, synthesis voices, events, and deterministic offline rendering.
It has no JACK, ALSA, signal, thread, process, or clock dependency.

The `shr-sampler` package owns the executable and host boundary:

```text
src/lib.rs             CLI validation; loads the complete instrument first
src/host/mod.rs        activation, MIDI thread, signals, and shutdown owner
src/host/jack.rs       dynamic JACK API, ports, and C callback boundary
src/host/midi.rs       pure MIDI-to-engine translation plus ALSA adaptation
src/host/queue.rs      fixed-capacity SPSC event handoff
src/host/callback.rs   bounded scheduling, rendering, silence, and fault state
```

The JACK API is resolved from `libjack.so.0` at runtime. Client opening always
passes `JACK_NO_START_SERVER | JACK_USE_EXACT_NAME`: an absent server is not
started and an existing same-name client makes startup fail instead of creating
a layered suffixed client. The process has no server-start, server-stop,
configuration, connection, or disconnection code. Build-time JACK headers are
therefore unnecessary. The maintained `alsa` crate is used only for ALSA
Sequencer capture. No ALSA PCM API is opened.

## Activation lifecycle

The executable performs these steps in order:

1. validate the exact live arguments and client name;
2. load the `.shrinst`, verify its package and sample hashes, decode every WAV,
   and prepare all package-owned data;
3. register SIGINT and SIGTERM flags;
4. dynamically open an existing JACK server, register `out_l` and `out_r`,
   read its sample rate, and construct the fixed engine and callback adapter;
5. open one nonblocking ALSA Sequencer capture client and create `input`;
6. create the MIDI thread in a stopped state;
7. activate the JACK client, then release the MIDI thread.

Any package error therefore occurs before JACK or ALSA activation. A later
setup error unwinds only resources owned by this process.

## Real-time boundary

The callback owns the prepared engine, fixed event array, one pending event,
queue consumer, output-port handles, and atomic status publishers. Its maximum
engine event slice is 256 events per period. Event insertion is stable and
bounded; equal-offset events retain FIFO order. A larger due backlog remains in
the queue and becomes offset zero if it is late in a later period.

The callback does not allocate or deallocate, lock, perform file or process
work, log, format, spawn, decode, or call ALSA. Package data, engine voices,
event storage, and both output buffers already exist. `Engine::render_block`
retains its exact-idle-silence and finite-output protection.

The MIDI thread samples JACK's `frames_since_cycle_start` when it receives a
translated event and schedules the event for the next observed callback cycle.
An offset beyond the period is clamped to its final frame; an event which has
become late is clamped to frame zero. This preserves host-provided sub-period
timing without accessing clocks or ALSA from the callback.

The software tests run the complete callback adapter and engine under an
allocation disabler. They also force the render error path and verify exact
zeroes in both provided stereo buffers. A callback fault stores only atomic
state in real time; the owner thread later formats and reports it.

## Ownership and shutdown

SIGINT, SIGTERM, an ALSA input error, a callback error, or JACK's shutdown
callback requests process-local shutdown. The owner stops MIDI intake, joins
the MIDI thread so the Sequencer port closes, requests callback All Notes Off,
waits at most 100 ms for its acknowledgement, deactivates its JACK client, and
performs a final race-free All Notes Off before closing the client and dynamic
library handle. JACK shutdown skips an unavailable callback acknowledgement
but still performs the final owned-state cleanup.

No path inspects, connects, reconfigures, signals, or terminates another JACK
client, ALSA port, process, or route.
