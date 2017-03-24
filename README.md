# Multi-consumer extensions to triple buffering ("SPMC buffer")

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

Compared to a triple buffer:

- Supports multiple consumers
- Consumes more CPU time and memory in the single-consumer case
- Not always wait-free for the writer. The guarantee can be offered, but it has
  large costs in terms of memory consumption for many readers.

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
- As the proper writing rate is system-dependent, what is configured in this
  test may not be appropriate for your machine.

Taking this and the relatively long run time (~10 s) into account, this test is
ignored by default.

To run the concurrent tests, make sure no one is eating CPU in the background,
then run the following command:

    $ cargo test --release -- --ignored --test-threads=1


## License

This crate is distributed under the terms of the LGPLv3 license. See the
LICENSE-LGPL-3.0.md file for details.

More relaxed licensing (Apache, MIT, BSD...) may also be negociated, in
exchange of a financial contribution. Contact me for details at 
knights_of_ni AT gmx DOTCOM.
