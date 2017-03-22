use std::cell::UnsafeCell;
use std::ops::BitAnd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A single-producer multiple-consumer buffer, useful for thread-safe data
/// sharing. More general variant of triple buffering for multiple consumers.
///
/// Triple buffering is an extremely efficient synchronization protocol when
/// a producer thread wants to constantly update a value that is visible by a
/// single consumer thread. However, it is not safe to use in the presence of
/// multiple consumers, because a consumer thread can no longer assume that it
/// is the only thread having access to the read buffer and discard said read
/// buffer at will.
///
/// Reference counting techniques can be used to build a variant of triple
/// buffering which works for multiple consumers, remains provably wait-free
/// if one uses two buffers per consumer, and degrades gracefully when a smaller
/// amount of buffers is used as long as consumers frequently fetch updates from
/// the producer. I call the resulting synchronization primitive an SPMC buffer.
///
#[derive(Debug)]
pub struct SPMCBuffer<T: Clone + PartialEq + Send> {
    /// Input object used by the producer to send updates
    input: SPMCBufferInput<T>,

    /// Clonable output object, used by consumers to read the current value
    output: SPMCBufferOutput<T>,
}
//
impl<T: Clone + PartialEq + Send> SPMCBuffer<T> {
    /// Construct an SPMC buffer allowing for wait-free writes under up to N
    /// concurrent readouts to distinct buffer versions
    pub fn new(wf_read_concurrency: usize, initial: T) -> Self {
        // Check that the amount of readers fits implementation limits
        assert!(wf_read_concurrency <= MAX_CONCURRENT_READERS);

        // Translate wait-free read concurrency into an actual buffer count
        let num_buffers = 2 * (1 + wf_read_concurrency);

        // Create the shared state. Buffer 0 is initially considered the latest,
        // and has one reader accessing it (corresponding to a refcount of 1).
        let shared_state =
            Arc::new(SPMCBufferSharedState {
                         buffers: vec![Buffer {
                                           data: UnsafeCell::new(initial),
                                           done_readers: AtomicRefCount::new(0),
                                       };
                                       num_buffers],
                         latest_info: AtomicSharedIndex::new(1),
                     });

        // ...then construct the input and output structs
        let mut result = SPMCBuffer {
            input: SPMCBufferInput {
                shared: shared_state.clone(),
                reader_counts: vec![0; num_buffers],
            },
            output: SPMCBufferOutput {
                shared: shared_state,
                read_idx: 0,
            },
        };

        // Mark the latest buffer with an "infinite" reference count, to forbid
        // selecting it as a write buffer (it's reader-visible!)
        result.input.reader_counts[0] = INFINITE_REFCOUNT;

        // Return the resulting valid SPMC buffer
        result
    }

    /// Extract input and output of the SPMC buffer
    pub fn split(self) -> (SPMCBufferInput<T>, SPMCBufferOutput<T>) {
        (self.input, self.output)
    }
}
//
// The Clone and PartialEq traits are used internally for testing.
//
impl<T: Clone + PartialEq + Send> Clone for SPMCBuffer<T> {
    fn clone(&self) -> Self {
        // Clone the shared state. This is safe because at this layer of the
        // interface, one needs an Input/Output &mut to mutate the shared state.
        let shared_state = Arc::new(unsafe { (*self.input.shared).clone() });

        // ...then the input and output structs
        SPMCBuffer {
            input: SPMCBufferInput {
                shared: shared_state.clone(),
                reader_counts: self.input.reader_counts.clone(),
            },
            output: SPMCBufferOutput {
                shared: shared_state,
                read_idx: self.output.read_idx,
            },
        }
    }
}
//
impl<T: Clone + PartialEq + Send> PartialEq for SPMCBuffer<T> {
    fn eq(&self, other: &Self) -> bool {
        // Compare the shared states. This is safe because at this layer of the
        // interface, one needs an Input/Output &mut to mutate the shared state.
        let shared_states_equal =
            unsafe { (*self.input.shared).eq(&*other.input.shared) };

        // Compare the rest of the triple buffer states
        shared_states_equal &&
        (self.input.reader_counts == other.input.reader_counts) &&
        (self.output.read_idx == other.output.read_idx)
    }
}


/// Producer interface to SPMC buffers
///
/// The producer can use this struct to submit updates to the SPMC buffer
/// whenever he likes. These updates may or may not be nonblocking depending
/// on the buffer size and the readout pattern.
///
#[derive(Debug)]
pub struct SPMCBufferInput<T: Clone + PartialEq + Send> {
    /// Reference-counted shared state
    shared: Arc<SPMCBufferSharedState<T>>,

    /// Amount of readers who potentially have access to each (unreachable)
    /// buffer. The latest buffer, which is still reachable, is marked with an
    /// "infinite" reference count, to warn that we don't know the true value.
    reader_counts: Vec<RefCount>,
}
//
impl<T: Clone + PartialEq + Send> SPMCBufferInput<T> {
    /// Write a new value into the SPMC buffer
    pub fn write(&mut self, value: T) {
        // Access the shared state
        let ref shared_state = *self.shared;

        // Go into a spin-loop, waiting for an "old" buffer with no live reader.
        // This loop will finish in a finite amount of iterations if each thread
        // is allocated two private buffers, because readers can hold at most
        // two buffers simultaneously. With less buffers, we may need to wait.
        let mut write_pos: Option<usize> = None;
        while write_pos == None {
            // We want to iterate over both buffers and associated refcounts
            let mut buf_rc_iter =
                shared_state.buffers.iter().zip(self.reader_counts.iter());

            // We want to find a buffer which is unreachable, and whose previous
            // readers have all moved on to more recent data. We identify
            // unreachable buffers by having previously tagged the latest buffer
            // with an infinite reference count.
            write_pos =
                buf_rc_iter.position(|tuple| {
                                        let (buffer, refcount) = tuple;
                                        *refcount == 
                                        buffer.done_readers
                                              .load(Ordering::Relaxed)
                                     });
        }
        let write_idx = write_pos.unwrap();

        // The buffer that we just obtained has been freed by old readers and is
        // unreachable by new readers, so we can safely allocate it as a write
        // buffer and put our new data into it
        let ref write_buffer = shared_state.buffers[write_idx];
        let write_ptr = write_buffer.data.get();
        unsafe {
            *write_ptr = value;
        }

        // No one has read this version of the buffer yet, so we reset all
        // reference-counting information to zero.
        write_buffer.done_readers.store(0, Ordering::Relaxed);

        // Publish our write buffer as the new latest buffer, and retrieve
        // the old buffer's shared index
        let former_latest_info = shared_state.latest_info.swap(
            write_idx * SHARED_INDEX_MULTIPLIER,
            Ordering::Release  // Publish updated buffer state to the readers
        );

        // In debug mode, make sure that overflow did not occur
        debug_assert!(former_latest_info.bitand(SHARED_OVERFLOW_BIT) == 0);

        // Decode the information contained in the former shared index
        let former_idx = former_latest_info.bitand(SHARED_INDEX_MASK) /
                         SHARED_INDEX_MULTIPLIER;
        let former_readcount = former_latest_info.bitand(SHARED_READCOUNT_MASK);

        // Write down the former buffer's refcount, and set the latest buffer's
        // refcount to infinity so that we don't accidentally write to it
        self.reader_counts[former_idx] = former_readcount;
        self.reader_counts[write_idx] = INFINITE_REFCOUNT;
    }
}


/// Consumer interface to SPMC buffers
///
/// A consumer of data can use this struct to access the latest published update
/// from the producer whenever he likes. Readout is nonblocking: a collision
/// between the producer and a consumer will result cache contention induced
/// slowdown, but deadlocks and scheduling-induced slowdowns cannot happen.
///
#[derive(Debug)]
pub struct SPMCBufferOutput<T: Clone + PartialEq + Send> {
    /// Reference-counted shared state
    shared: Arc<SPMCBufferSharedState<T>>,

    /// Index of the buffer which the consumer is currently reading from
    read_idx: BufferIndex,
}
//
impl<T: Clone + PartialEq + Send> SPMCBufferOutput<T> {
    /// Access the latest value from the SPMC buffer
    pub fn read(&mut self) -> &T {
        // Access the shared state
        let ref shared_state = *self.shared;

        // Check if the producer has submitted an update
        let latest_info = shared_state.latest_info.load(Ordering::Relaxed);
        let update_available = latest_info.bitand(SHARED_INDEX_MASK) !=
                               (self.read_idx * SHARED_INDEX_MULTIPLIER);

        // If so, drop our current read buffer and go with the latest buffer
        if update_available {
            // Acquire access to the latest buffer, incrementing its
            // refcount to tell the producer that we have access to it
            let latest_info = shared_state.latest_info.fetch_add(
                1,
                Ordering::Acquire  // Fetch the associated buffer state
            );

            // Drop our current read buffer. Because we already used an acquire
            // fence above, we can safely use relaxed atomic order here: no CPU
            // or compiler will reorder this operation before the fence.
            unsafe {
                self.discard_read_buffer(Ordering::Relaxed);
            }

            // In debug mode, make sure that overflow did not occur
            debug_assert!((latest_info + 1).bitand(SHARED_OVERFLOW_BIT) == 0);

            // Extract the index of our new read buffer
            self.read_idx = latest_info.bitand(SHARED_INDEX_MASK) /
                            SHARED_INDEX_MULTIPLIER;
        }

        // Access data from the current (read-only) read buffer
        let read_ptr = shared_state.buffers[self.read_idx].data.get();
        unsafe { &*read_ptr }
    }

    /// Drop the current read buffer. This is unsafe because it allows the
    /// writer to write into it, which means that the read buffer must never be
    /// accessed again after this operation completes. Be extremely careful with
    /// memory ordering: this operation must NEVER be reordered before a read!
    unsafe fn discard_read_buffer(&self, order: Ordering) {
        self.shared.buffers[self.read_idx].done_readers.fetch_add(1, order);
    }
}
//
impl<T: Clone + PartialEq + Send> Clone for SPMCBufferOutput<T> {
    // Create a new output interface associated with a given SPMC buffer
    fn clone(&self) -> Self {
        // Clone the current shared state
        let shared_state = self.shared.clone();

        // Acquire access to the latest buffer, incrementing its refcount
        let latest_info = shared_state.latest_info.fetch_add(
            1,
            Ordering::Acquire  // Fetch the associated buffer state
        );

        // Extract the index of this new read buffer
        let new_read_idx = latest_info.bitand(SHARED_INDEX_MASK) /
                           SHARED_INDEX_MULTIPLIER;

        // Build a new output interface from this information
        SPMCBufferOutput {
            shared: shared_state,
            read_idx: new_read_idx,
        }
    }
}
//
impl<T: Clone + PartialEq + Send> Drop for SPMCBufferOutput<T> {
    // Discard our read buffer on thread exit
    fn drop(&mut self) {
        // We must use release ordering here in order to prevent preceding
        // buffer reads from being reordered after the buffer is discarded
        unsafe {
            self.discard_read_buffer(Ordering::Release);
        }
    }
}


/// Shared state for SPMC buffers
///
/// This struct provides both a set of shared buffers for single-producer
/// multiple-consumer broadcast communication and a way to know which of these
/// buffers contains the most up to date data with reader reference counting.
///
/// The number of buffers N is a design tradeoff: the larger it is, the more
/// robust the primitive is against contention, at the cost of increased memory
/// usage. An SPMC buffer is provably wait-free for both readers and writers if
/// N = Nreaders + 3, where Nreaders is the amount of data consumers, but it
/// can work correctly in a degraded regime which is wait-free for readers and
/// potentially blocking for writers as long as N >= 2.
///
/// Note that for 1 reader, we need 4 buffers to be provably wait-free, rather
/// than 3 in the case of triple buffering. The explanation for this boils down
/// to the fact that we need to use two separate atomic variables to signal
/// incoming and departing readers, which means that atomic buffer swap is not
/// available anymore, and thus that the writer can observe a state where a
/// reader has access to a new buffer, but not yet discarded the previous one.
///
#[derive(Debug)]
struct SPMCBufferSharedState<T: Clone + PartialEq + Send> {
    /// Data storage buffers
    buffers: Vec<Buffer<T>>,

    /// Combination of reader count and latest buffer index (see below)
    latest_info: AtomicSharedIndex,
}
//
impl<T: Clone + PartialEq + Send> SPMCBufferSharedState<T> {
    /// Cloning the shared state is unsafe because you must ensure that no one
    /// is concurrently accessing it, since &self is enough for writing.
    unsafe fn clone(&self) -> Self {
        SPMCBufferSharedState {
            buffers: self.buffers.clone(),
            latest_info: AtomicSharedIndex::new(
                self.latest_info.load(Ordering::Relaxed)
            ),
        }
    }

    /// Equality is unsafe for the same reason as cloning: you must ensure that
    /// no one is concurrently accessing the triple buffer to avoid data races.
    unsafe fn eq(&self, other: &Self) -> bool {
        // Determine whether the contents of all buffers are equal
        let buffers_equal = self.buffers
            .iter()
            .zip(other.buffers.iter())
            .all(|tuple| -> bool {
                     let (buf1, buf2) = tuple;
                     let dr1 = buf1.done_readers.load(Ordering::Relaxed);
                     let dr2 = buf2.done_readers.load(Ordering::Relaxed);
                     (*buf1.data.get() == *buf2.data.get()) && (dr1 == dr2)
                 });

        // Use that to deduce if the entire shared state is equivalent
        buffers_equal &&
        (self.latest_info.load(Ordering::Relaxed) ==
         other.latest_info.load(Ordering::Relaxed))
    }
}
//
unsafe impl<T: Clone + PartialEq + Send> Sync for SPMCBufferSharedState<T> {}
//
//
#[derive(Debug)]
struct Buffer<T: Clone + PartialEq + Send> {
    /// Actual data must be in an UnsafeCell so that Rust knows it's mutable
    data: UnsafeCell<T>,

    /// Amount of readers who are done with this buffer and switched to another
    done_readers: AtomicRefCount,
}
//
impl<T: Clone + PartialEq + Send> Clone for Buffer<T> {
    /// WARNING: Buffers are NOT safe to clone, because a writer might be
    ///          concurrently writing to them. The only reason why I'm not
    ///          marking this function as unsafe is Rust would then not accept
    ///          it as a Clone implementation, which would make Vec manipulation
    ///          a lot more painful.
    fn clone(&self) -> Self {
        Buffer {
            data: UnsafeCell::new(unsafe { (*self.data.get()).clone() }),
            done_readers: AtomicRefCount::new(self.done_readers
                                                  .load(Ordering::Relaxed)),
        }
    }
}


/// Atomic "shared index", combining "latest buffer" and "reader count" info
/// in a single large integer through silly bit tricks.
///
/// At the start of the readout process, a reader must atomically announce
/// itself as in the process of reading the current buffer (so that said buffer
/// does not get reclaimed) and determine which buffer is the current buffer.
///
/// Here is why these operations cannot be separated:
///
/// - Assume that the reader announces that it is reading, then determines which
///   buffer is the current buffer. In this case, the reader can only make the
///   generic announcement that it is reading "some" buffer, because it does not
///   know yet which buffer it'll be reading. This means that other threads do
///   not know which buffers are busy, and no buffer can be liberated until the
///   reader clarifies its intent or goes away. This way of operating is thus
///   effectively equivalent to a reader-directed update lock.
/// - Assume that the reader determines which buffer is the current buffer, then
///   announces itself as being in the process of reading this specific buffer.
///   Inbetween these two actions, the current buffer may have changed, so the
///   reader may increment the wrong refcount. Furthermore, the buffer that is
///   now targeted by the reader may have already be tagged as safe for reuse or
///   deletion by the writer, so if the reader proceeds with reading it, it may
///   accidentally end up in a data race with the writer. This follows the
///   classical rule of thumb that one should always reserve resources before
///   accessing them, however lightly.
///
/// To combine latest buffer index readout and reader count increment, we need
/// to pack both of these quantities into a single shared integer variable that
/// we can manipulate through a atomic operations. For refcounting, fetch_add
/// sounds like a good choice, so we want an atomic integer type whose low-order
/// bits act as a refcount and whose high-order bit act as a buffer index.
/// Here's an example for a 16-bit unsigned integer, allowing up to 64 buffers
/// and 511 concurrent readers on each buffer:
///
///   bit (high-order first):       15 .. 10  9  8 .. 0
///                                +--------+--+-------+
///   Contents:                    |BUFFERID|OF|READCNT|
///                                +--------+--+-------+
///
/// In this scheme, BUFFERID is the index of the "latest buffer", which contains
/// the newest data from the writer, and READCNT is the amount of readers who
/// have acquired access to this data. In principle, the later counter could
/// overflow in the presence of 512+ concurrent readers, all accessing the same
/// buffer without a single update happening in meantime. This scenario is
/// highly implausible on current hardware architectures (even many-core ones),
/// but we nevertheless account for it by adding an overflow "OF" bit, which is
/// checked in debug builds. A thread which detects such overflow should panic.
///
/// TODO: Switch to U16 / AtomicU16 once the later is stable
///
type BufferIndex = usize;
//
type RefCount = usize;
const INFINITE_REFCOUNT: RefCount = 0xffff;
type AtomicRefCount = AtomicUsize;
//
type SharedIndex = usize;
type AtomicSharedIndex = AtomicUsize;
const SHARED_READCOUNT_MASK:   SharedIndex = 0b0000_0001_1111_1111;
const SHARED_OVERFLOW_BIT:     SharedIndex = 0b0000_0010_0000_0000;
const SHARED_INDEX_MASK:       SharedIndex = 0b1111_1100_0000_0000;
const SHARED_INDEX_MULTIPLIER: SharedIndex = 0b0000_0100_0000_0000;
//
const MAX_BUFFERS: usize = SHARED_INDEX_MASK / SHARED_INDEX_MULTIPLIER + 1;
const MAX_CONCURRENT_READERS: usize = MAX_BUFFERS / 2 - 1;


/// Unit tests
#[cfg(test)]
mod tests {
    use std::ops::BitAnd;
    use std::sync::{Arc, Condvar, Mutex};
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;

    /// Check that SPMC buffers are properly initialized as long as the
    /// requested amount of concurrent readers stays in implementation limits.
    #[test]
    fn initial_state() {
        // Test for 0 readers (writer-blocking double-buffering limit)
        test_initialization(0);

        // Test for 1 concurrent reader (quadruple buffering)
        test_initialization(1);

        // Test for maximal amount of concurrent readers
        test_initialization(::MAX_CONCURRENT_READERS);
    }

    /// Check that SPMC buffer initialization panics if too many readers are
    /// requested with respect to implementation limits.
    #[test]
    #[should_panic]
    fn too_many_readers() {
        test_initialization(::MAX_CONCURRENT_READERS + 1);
    }

    /// Check that writing to an SPMC buffer works, but can be blocking
    #[test]
    fn write_write_sequence() {
        // Let's create a double buffer
        let mut buf = ::SPMCBuffer::new(0, 1.0);

        // Backup the initial buffer state
        let old_buf = buf.clone();

        // Perform a write
        buf.input.write(4.2);

        // Analyze the new buffer state
        {
            // Starting from the old buffer state...
            let mut expected_buf = old_buf.clone();
            let ref expected_shared = expected_buf.input.shared;

            // We expect the buffer which is NOT accessed by the current reader
            // to have received the new value from the writer.
            let old_read_idx = old_buf.output.read_idx;
            let write_idx = 1 - old_read_idx;
            let write_ptr = expected_shared.buffers[write_idx].data.get();
            unsafe {
                *write_ptr = 4.2;
            }

            // We expect the latest buffer information to now point towards
            // this write buffer
            let new_latest_info = write_idx * ::SHARED_INDEX_MULTIPLIER;
            expected_shared.latest_info.store(new_latest_info,
                                              Ordering::Relaxed);

            // We expect the writer to have marked this write index as
            // unreachable, since it is now reader-visible, and to have fetched
            // the reference count of the former read buffer
            expected_buf.input.reader_counts[write_idx] = ::INFINITE_REFCOUNT;
            expected_buf.input.reader_counts[old_read_idx] = 1;

            // Nothing else should have changed
            assert_eq!(buf, expected_buf);
        }

        // At this point, all buffers are busy: the reader holds one buffer, and
        // the other is publicly visible. So trying to commit another write
        // should lead the writer into a waiting loop, from which it can only
        // exit if the reader drops its current buffer. Let's check that.
        {
            // Prepare some synchronization structures to follow writer progress
            let sync = Arc::new((Mutex::new(0), Condvar::new()));
            let writer_sync = sync.clone();

            // Send a thread on a suicide mission to write into the buffer
            let (mut buf_input, mut buf_output) = buf.split();
            let writer = thread::spawn(move || {
                                           *writer_sync.0.lock().unwrap() = 1;
                                           buf_input.write(2.4);
                                           *writer_sync.0.lock().unwrap() = 2;
                                           writer_sync.1.notify_all();
                                       });

            // Wait a bit to make sure that the writer cannot proceed
            let shared_lock = sync.0.lock().unwrap();
            let wait_result =
                sync.1.wait_timeout(shared_lock, Duration::from_millis(100));
            let (shared_lock, timeout_result) = wait_result.unwrap();
            assert!(timeout_result.timed_out());
            assert_eq!(*shared_lock, 1);

            // Make the reader check out the new buffer state, freeing the
            // buffer that it was previously holding
            let _ = buf_output.read();

            // Check that the writer can now proceed
            let wait_result =
                sync.1.wait_timeout(shared_lock, Duration::from_millis(100));
            let (shared_lock, timeout_result) = wait_result.unwrap();
            assert!(!timeout_result.timed_out());
            assert_eq!(*shared_lock, 2);

            // Wait for the writer to finish
            writer.join().unwrap();
        }
    }

    // TODO: Check that reading from an SPMC buffer works (write-read-read)
    // TODO: Check other write/read scenarios (think about possible code paths)
    // TODO: Check that spawning a new reader and using it works
    // TODO: Check that the writer waits for readers if needed
    // TODO: Check that concurrent reads and writes work

    /// Try initializing a buffer for some maximal wait-free readout concurrency
    fn test_initialization(wf_conc_readers: usize) {
        // Create a buffer with the requested wait-free read concurrency
        let buf = ::SPMCBuffer::new(wf_conc_readers, 42);

        // Access the shared state
        let ref buf_shared = *buf.input.shared;

        // Check that we have an appropriate amount of buffers
        let num_buffers = buf_shared.buffers.len();
        assert_eq!(num_buffers, 2 * (1 + wf_conc_readers));

        // Decode and check the latest buffer metadata: we should have one
        // reader, no refcount overflow, and a valid latest buffer index
        let latest_info = buf_shared.latest_info.load(Ordering::Relaxed);
        let reader_count = latest_info.bitand(::SHARED_READCOUNT_MASK);
        assert_eq!(reader_count, 1);
        let overflow = latest_info.bitand(::SHARED_OVERFLOW_BIT) != 0;
        assert!(!overflow);
        let latest_idx = latest_info.bitand(::SHARED_INDEX_MASK) /
                         ::SHARED_INDEX_MULTIPLIER;
        assert!(latest_idx < num_buffers);

        // The reader must initially use the latest buffer as a read buffer
        assert_eq!(buf.output.read_idx, latest_idx);

        // The read buffer must be properly initialized
        let ref buffers = buf_shared.buffers;
        let read_ptr = buffers[latest_idx].data.get();
        assert_eq!(unsafe { *read_ptr }, 42);

        // The outgoing reader count of each buffer must be 0 initially.
        for buffer in buffers {
            assert_eq!(buffer.done_readers.load(Ordering::Relaxed), 0);
        }

        // Every buffer except for the read buffer should be considered free
        // in the writer's internal reference counting records. The read buffer
        // should use a special infinite refcount to completely forbid writing.
        let indexes_and_refcounts = buf.input
            .reader_counts
            .iter()
            .enumerate();
        for tuple in indexes_and_refcounts {
            let (index, refcount) = tuple;
            if index != latest_idx {
                assert_eq!(*refcount, 0);
            } else {
                assert_eq!(*refcount, ::INFINITE_REFCOUNT);
            }
        }
    }
}


// TODO: Add performance benchmarks
