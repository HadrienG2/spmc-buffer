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
pub struct SPMCBuffer<T: Clone> {
    /// Input object used by the producer to send updates
    input: SPMCBufferInput<T>,

    /// Clonable output object, used by consumers to read the current value
    output: SPMCBufferOutput<T>,
}
//
impl<T: Clone> SPMCBuffer<T> {
    /// Construct an SPMC buffer allowing for wait-free writes under up to N
    /// concurrent readouts to distinct versions, and with some initial content.
    pub fn new(max_conc_readers: usize, initial: T) -> Self {
        // Translate max_conc_readers into an actual buffer count
        let num_buffers = 2 + 2*max_conc_readers;

        // Check that this count is compatible with the current implementation
        assert!((num_buffers-1)*SHARED_INDEX_MULTIPLIER <= SHARED_INDEX_MASK);

        // Create the shared state. Buffer 0 is initially considered the latest.
        let shared_state = Arc::new(
            SPMCBufferSharedState {
                buffers: vec![
                    Buffer {
                        data: UnsafeCell::new(initial),
                        done_readers: AtomicRefCount::new(0),
                    };
                    num_buffers
                ],
                latest_buf: AtomicSharedIndex::new(0),
            }
        );

        // ...then construct the input and output structs. Any consumer should
        // be initialized with an invalid read index so that it increments the
        // refcount of the latest buffer on its first read.
        let mut result = SPMCBuffer {
            input: SPMCBufferInput {
                shared: shared_state.clone(),
                reader_counts: vec![0; num_buffers],
            },
            output: SPMCBufferOutput {
                shared: shared_state,
                read_idx: INVALID_INDEX,
            },
        };

        // Mark the latest buffer with an "infinite" reference count, to forbid
        // using it as a write buffer (it's reader-visible!)
        result.input.reference_counts[0] = INFINITE_REFCOUNT;

        // Return the resulting valid SPMC buffer
        result
    }

    /// Extract input and output of the SPMC buffer
    pub fn split(self) -> (SPMCBufferInput<T>, SPMCBufferOutput<T>) {
        (self.input, self.output)
    }
}


/// Producer interface to SPMC buffers
///
/// The producer can use this struct to submit updates to the SPMC buffer
/// whenever he likes. These updates may or may not be nonblocking depending
/// on the buffer size and the readout pattern.
///
pub struct SPMCBufferInput<T: Clone> {
    /// Reference-counted shared state
    shared: Arc<SPMCBufferSharedState<T>>,

    /// Amount of readers who potentially have access to each (unreachable)
    /// buffer. The latest buffer, which is still reachable, is marked with an
    /// "infinite" reference count, to warn that we don't know the true value.
    reader_counts: Vec<RefCount>,
}
//
impl<T: Clone> SPMCBufferInput<T> {
    /// Write a new value into the SPMC buffer
    pub fn write(&mut self, value: T) {
        // Access the shared state
        let ref shared_state = *self.shared;

        // Go into a spin-loop, waiting for an "old" buffer with no live reader.
        // This loop must finish in a finite amount of iterations if each thread
        // is allocated two private buffers, because readers can hold at most
        // two buffers simultaneously. With less buffers, we may need to wait.
        let mut write_pos: Option<usize> = None;
        while write_pos == None {
            // We want to iterate over both buffers and associated refcounts
            let mut buf_rc_iter =
                shared_state.buffers.iter()
                                    .zip(self.reader_counts.iter());

            // We want to find a buffer which is unreachable, and whose previous
            // readers have all moved on to more recent data. We identify
            // unreachable buffers by having previously tagged the latest buffer
            // with an infinite reference count.
            write_pos = buf_rc_iter.position(|tuple| {
                let (buffer, refcount) = tuple;
                let done_readers = buffer.done_readers.load(Ordering::Relaxed);
                done_readers == *refcount
            });
        }
        let write_idx = write_pos.unwrap();

        // The buffer that we just obtained is unused by old readers and
        // unreachable by new readers, so we can safely allocate it as a write
        // buffer and put our new data into it
        let ref write_buffer = shared_state.buffers[write_idx];
        let write_ptr = write_buffer.data.get();
        unsafe { *write_ptr = value; }

        // No one has read this version of the buffer yet, so we reset all
        // reference-counting information to zero.
        write_buffer.done_readers.store(0, Ordering::Relaxed);

        // Publish our write buffer as the new latest buffer, and retrieve
        // the old buffer's shared index
        let former_latest_buf = shared_state.latest_buf.swap(
            write_idx * SHARED_INDEX_MULTIPLIER,
            Ordering::Release  // Publish updated buffer state to the readers
        );

        // In debug mode, make sure that overflow did not occur
        debug_assert!(former_latest_buf.bitand(SHARED_OVERFLOW_BIT) == 0);

        // Decode the information contained in the former shared index
        let former_idx = former_latest_buf.bitand(SHARED_INDEX_MASK)
                                          / SHARED_INDEX_MULTIPLIER;
        let former_readcount = former_latest_buf.bitand(SHARED_READCOUNT_MASK);

        // Write down the former buffer's refcount, and set the latest buffer's
        // refcount to infinity so that we don't accidentally write to it
        self.reference_counts[former_idx] = former_readcount;
        self.reference_counts[write_idx] = INFINITE_REFCOUNT;
    }                      
}


/// Consumer interface to SPMC buffers
///
/// A consumer of data can use this struct to access the latest published update
/// from the producer whenever he likes. Readout is nonblocking: a collision
/// between the producer and a consumer will result cache contention induced
/// slowdown, but deadlocks and scheduling-induced slowdowns cannot happen.
///
pub struct SPMCBufferOutput<T: Clone> {
    /// Reference-counted shared state
    shared: Arc<SPMCBufferSharedState<T>>,

    /// Index of the buffer which the consumer is currently reading from
    read_idx: BufferIndex,
}
//
impl<T: Clone> SPMCBufferOutput<T> {
    /// Access the latest value from the SPMC buffer
    pub fn read(&mut self) -> &T {
        // Access the shared state
        let ref shared_state = *self.shared;

        // Check if the producer has submitted an update
        let latest_buf = shared_state.latest_buf.load(Ordering::Relaxed);
        let update_available =
            latest_buf.bitand(SHARED_INDEX_MASK)
                != self.read_idx * SHARED_INDEX_MULTIPLIER;

        // If so, exchange our current read buffer with the latest buffer
        if update_available {
            // Acquire access to the latest buffer, incrementing its
            // refcount to tell the producer that we have access to it
            let latest_buf = shared_state.latest_buf.fetch_add(
                1,
                Ordering::Acquire  // Fetch the associated buffer state
            );

            // Drop our current read buffer. Because we already used an acquire
            // fence above, we can safely use relaxed atomic order here: no CPU
            // or compiler will reorder this operation before the fence.
            unsafe { self.discard_read_buffer(Ordering::Relaxed); }

            // In debug mode, make sure that overflow did not occur
            debug_assert!((latest_buf+1).bitand(SHARED_OVERFLOW_BIT) == 0);

            // Extract the index of our new read buffer
            self.read_idx = latest_buf.bitand(SHARED_INDEX_MASK)
                                      / SHARED_INDEX_MULTIPLIER;
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
impl<T: Clone> Clone for SPMCBufferOutput<T> {
    // Create a new reader associated with a given SPMC buffer
    fn clone(&self) -> Self {
        SPMCBufferOutput {
            shared: self.shared.clone(),
            read_idx: INVALID_INDEX,
        }
    }
}
//
impl<T: Clone> Drop for SPMCBufferOutput<T> {
    // Discard our read buffer on thread exit
    fn drop(&mut self) {
        // We must use release ordering here in order to prevent preceding
        // buffer reads from being reordered after the buffer is discarded
        unsafe { self.discard_read_buffer(Ordering::Release); }
    }
}


/// Shared state for SPMC buffers
///
/// This struct provides both a set of shared buffers for single-producer
/// multiple-consumer broadcast communication and 
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
struct SPMCBufferSharedState<T: Clone> {
    /// Data storage buffers
    buffers: Vec<Buffer<T>>,

    /// Combination of reader count and latest buffer index (see below)
    latest_buf: AtomicSharedIndex,
}
//
struct Buffer<T: Clone> {
    /// Actual data must be in an UnsafeCell so that Rust knows it's mutable
    data: UnsafeCell<T>,

    /// Amount of readers who are done with this buffer and switched to another
    done_readers: AtomicRefCount,
}
//
impl<T: Clone> Clone for Buffer<T> {
    /// WARNING: Buffers are NOT safe to clone, because a writer might be
    ///          concurrently writing to them. The only reason why I'm not
    ///          marking this function as unsafe is that Rust won't allow me to
    ///          while staying compatible with the vec! single-item constructor
    fn clone(&self) -> Self {
        Buffer {
            data: UnsafeCell::new(
                unsafe { (*self.data.get()).clone() }
            ),
            done_readers: AtomicRefCount::new(
                self.done_readers.load(Ordering::Relaxed)
            ),
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
const INVALID_INDEX: BufferIndex = 0xffff;
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


// TODO: Add tests
#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
    }
}
