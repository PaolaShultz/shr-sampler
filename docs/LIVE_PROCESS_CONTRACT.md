# Live process contract

## Invocation and public ports

The live executable form is exact:

```sh
shr-sampler --client-name NAME --instrument /path/to/file.shrinst
```

`NAME` is supplied by the process owner and must contain 1 through 63
non-control UTF-8 bytes. It is used as both the JACK and ALSA Sequencer client
name. JACK requires the exact name and refuses a collision rather than adding a
suffix. Public short port names are stable:

- JACK audio output: `out_l`
- JACK audio output: `out_r`
- ALSA Sequencer MIDI input: `input`

There are exactly two JACK audio ports. The host creates no JACK connections
and opens no ALSA audio device.

## MIDI semantics

All MIDI channels use the same instrument engine:

- note-on maps note and integer velocity directly;
- velocity-zero note-on maps to note-off;
- note-off releases every matching gated voice according to engine policy;
- CC64 values 0–63 release sustain and 64–127 engage it;
- CC120 (All Sound Off), CC123 (All Notes Off), and ALSA Reset map to immediate
  engine All Notes Off;
- other controllers and unsupported event types are ignored.

All Notes Off clears sustain and every owned voice immediately. It is also the
overflow-recovery and shutdown operation.

## Queue and callback bounds

The SPSC MIDI handoff holds exactly 1,024 `ScheduledEvent` values. It is
preallocated, lock-free, and nonblocking. If full, the producer drops the
newest event, increments an overflow counter, and publishes a recovery request.
At the next callback the consumer forgets the existing backlog in constant time
and performs All Notes Off before rendering. This policy prevents a dropped
note-off from leaving a voice or sustain state hung; later events can enter the
recovered queue normally.

At most 256 events are passed to `Engine::render_block` in one period. Due
events are ordered by clamped sample offset while equal offsets retain arrival
order. Events beyond the cap remain queued rather than being discarded. Queue
overflow recovery counts and period-cap deferral counts are formatted and
reported only by the non-real-time owner thread.

## Fault and shutdown behavior

An engine render failure clears both supplied JACK output buffers to exact
`0.0`, performs All Notes Off, atomically publishes the first fault, and asks
the owner to shut down. Failure to obtain a JACK buffer clears every buffer
which JACK supplied, performs All Notes Off, publishes a buffer fault, and
also requests shutdown. No callback fault is logged or formatted in real time.

SIGINT and SIGTERM are clean success paths. JACK shutdown, ALSA input failure,
buffer failure, and engine render failure clean up owned resources and return a
specific process error. Shutdown never starts, stops, restarts, reconfigures,
or connects JACK and never alters an unrelated process, client, port, or route.

## Evidence boundary

Deterministic hardware-free tests verify queue capacity/order/overflow and
recovery, MIDI translation and thresholds, timing order/clamping, the
per-period cap, exact fault silence, fault/shutdown publication, callback and
engine allocation freedom, live argument validation, pre-activation package
rejection, and public names. Existing package, SFZ, engine, offline, and CLI
tests remain in the normal suite.

Connected discovery, real JACK/ALSA event timing, signal behavior under a live
server, callback headroom, safe polyphony, latency, routing, audio quality, and
Raspberry Pi performance remain future explicitly authorized evidence.
