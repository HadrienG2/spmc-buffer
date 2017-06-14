# SPMC Buffer: Triple-buffering for multiple consumers

[![On crates.io](https://img.shields.io/crates/v/spmc_buffer.svg)](https://crates.io/crates/spmc_buffer)
[![On docs.rs](https://docs.rs/spmc_buffer/badge.svg)](https://docs.rs/spmc_buffer/)
[![Build status](https://travis-ci.org/HadrienG2/spmc-buffer.svg?branch=master)](https://travis-ci.org/HadrienG2/spmc-buffer)

## What is this?

This is an extension of my earlier work on triple buffering, which supports
readout from multiple consumers at the cost of some extra memory and CPU
overhead. You may find it useful for the following class of thread
synchronization problems:

- There is one producer thread and several consumer threads
- The producer wants to update a shared memory value periodically
- The consumers wants to access the latest update from the producer at any time

It is currently used as follows:

```rust
// Create an SPMC buffer of any Clone type
use spmc_buffer::SPMCBuffer;
let buf = SPMCBuffer::new(2, 1.0);

// Split it into an input and output interface
let (mut buf_input, mut buf_output) = buf.split();

// Create as many extra output interfaces as needed
let mut buf_output2 = buf_output.clone();

// The producer can move a value into the buffer at any time
buf_input.write(4.2);

// A consumer can access the latest value from the producer at any time
let mut latest_value_ref = buf_output.read();
assert_eq!(*latest_value_ref, 4.2);
let latest_value_ref2 = buf_output2.read();
assert_eq!(*latest_value_ref2, 4.2);
```


## Give me details! How does it compare to alternatives?

Compared to a triple buffer an SPMC buffer...

- Supports multiple consumers (that's the point!)
- Consumes more CPU time and memory in the single-consumer case
- Is not always wait-free for the writer. The guarantee can be offered, but it
  has quite large memory costs for many readers.

In short, SPMC buffering is what you're after in scenarios where a shared
memory location is updated frequently by a single writer, read by multiple
reader who only wants the latest version, and you can spare some RAM.

- If you need multiple producers, look somewhere else
- If you only need one consumer, use a triple buffer instead
- If you can't tolerate the RAM overhead or want to update the data in place,
  try a Mutex instead (or possibly an RWLock)
- If the shared value is updated very rarely (e.g. every second), try an RCU
- If the consumer must get every update, try a message queue


## How do I know your unsafe lock-free code is working?

By running the tests, of course! Which is unfortunately currently harder than
I'd like it to be.

First of all, we have sequential tests, which are very thorough but obviously
do not check the lock-free/synchronization part. You run them as follows:

    $ cargo test --release

Then we have concurrent tests, where we fire up concurrent readers and writer
threads and check that the readers can never observe an inconsistent buffer
state. These tests are more important, but they are also harder to run because
one must first check some assumptions:

- The testing host must have at least 3 physical CPU cores to test all possible
  race conditions
- No other code should be eating CPU in the background. Including other tests.
- Some tests have timing-dependent behaviour, and may require manual tweaking
  of sleep periods for your specific system.

Taking this and the relatively long run time (~10 s) into account, these tests
are ignored by default.

Finally, we have benchmarks, which allow you to test how well the code is
performing on your machine. Because cargo bench has not yet landed in Stable
Rust, these benchmarks masquerade as tests, which make them a bit unpleasant to
run. I apologize for the inconvenience.

To run the concurrent tests and the benchmarks, make sure no one is eating CPU
in the background and do:

    $ cargo test --release -- --ignored --test-threads=1

Here is a guide to interpreting the benchmark results:

* `clean_read` measures the triple buffer readout time when the data has not
  changed. It should be extremely fast (a couple of CPU clock cycles).
* `write` measures the amount of time it takes to write data in the triple
  buffer when no one is reading.
* `write_and_dirty_read` performs a write as before, immediately followed by a
  sequential read. To get the dirty read performance, substract the write time
  from that result. Writes and dirty read should take comparable time.
* `concurrent_write` measures the write performance when a reader is
  continuously reading. Expect significantly worse performance: lock-free
  techniques can help against contention, but are not a panacea.
* `concurrent_read` measures the read performance when a writer is continuously
  writing. Again, a significant hit is to be expected.

On my laptop's CPU (Intel Core i7-4720HQ), typical results are as follows:

* Write: 12 ns
* Clean read: 1.3 ns
* Dirty read: 17 ns
* Concurrent write: 100 ns
* Concurrent read: 38 ns


## License

This crate is distributed under the terms of the LGPLv3 license. See the
LICENSE-LGPL-3.0.md file for details.

More relaxed licensing (Apache, MIT, BSD...) may also be negociated, in
exchange of a financial contribution. Contact me for details at 
knights_of_ni AT gmx DOTCOM.
