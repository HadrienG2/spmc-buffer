use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A single-producer multiple-consumer buffer, useful for thread-safe data
/// sharing. More general variant of triple buffering for multiple consumers.
///
/// Triple buffering is an extremely efficient synchronization protocol when
/// a producer thread wants to constantly update a value that is visible by a
/// single consumer thread. However, it is not safe to use in the presence of
/// multiple consumers, because the consumer thread can no longer assume that
/// it is the only thread having access to the read buffer.
///
/// Reference counting techniques can be used to build a multi-consumer variant
/// of triple buffering which works for multiple consumers, remains provably
/// wait-free if one extra buffer per extra consumer is added, and degrades
/// gracefully when a smaller amount of buffers is used. I call the resulting
/// construct an SPMC buffer.
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
    /// concurrent readouts to distinct buffers, and with some initial content.
    pub fn new(max_conc_readers: usize, initial: T) -> Self {
        // Translate max_conc_readers into an actual buffer count
        assert!(max_conc_readers > 0);
        let num_buffers = max_conc_readers + 2;

        // Start with the shared state...
        let shared_state = Arc::new(
            SPMCBufferSharedState {
                storage: vec![
                    Buffer {
                        data: UnsafeCell::new(initial),
                        done_readers: AtomicRefcount::new(0),
                    };
                    num_buffers
                ],
                latest_buf: AtomicSharedIndex::new(0),
            }
        );

        // ...then construct the input and output structs
        let mut result = SPMCBuffer {
            input: SPMCBufferInput {
                shared: shared_state.clone(),
                reference_counts: vec![0; num_buffers],
            },
            output: SPMCBufferOutput {
                shared: shared_state,
                read_idx: INVALID_INDEX,
            },
        };

        // Don't forget to mark the currently published buffer as busy, so that
        // the producer doesn't get the silly idea of writing into it.
        result.input.reference_counts[0] = UNKNOWN_REFCOUNT;

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

    /// Amount of readers who potentially have access to each buffer.
    /// The latest buffer has a maximal reference count to clarify that its
    /// actual number of readers is unknown and potentially very high.
    reference_counts: Vec<Refcount>,
}

// TODO: Writer algorithm:
//          - We know which buffer is the latest buffer, and shouldn't touch it
//          - Scan private list of other buffers' read counts, to find a buffer
//            whose readers are all done (readcount = donereads)
//          - Write new data into this "safe" buffer, which has no readers
//          - When done, swap latest_idx with the latest buffer's index and
//            a readcount of zero
//          - Check if read count overflow occured, and if so panic
//          - Otherwise, memorize the old read count in our private list


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

// TODO: Reader algorithm:
//          - Keep track of the last buffer we've read
//          - Relaxed-check the latest_buf AtomicSharedIdx to see if a new
//            buffer is available
//          - If not, we're done, otherwise drop our buffer
//          - Fetch-add the latest_buf AtomicSharedIdx to acquire a new buffer
//          - Check if read count overflow occured, and if so panic
//          - Extract buffer ID
//          - Provide a buffer smart-ref that allows required read access
//          - When the smart-ref is dropped, increment buffer's done-read count
//              (Decrementing the readcount is unsafe because the writer may
//              have commited an update to latest_idx meanwhile)


/// Shared state for SPMC buffers
///
/// This struct provides both a set of shared buffers for single-producer
/// multiple-consumer broadcast communication and 
///
/// The number of buffers N is a design tradeoff: the larger it is, the more
/// robust the primitive is against contention, at the cost of increased memory
/// usage. An SPMC buffer is provably wait-free for both readers and writers if
/// N >= Nreaders + 2, where Nreaders is the amount of data consumers, but it
/// can work correctly in a degraded regime which is wait-free for readers and
/// potentially blocking for writers as long as N >= 3.
///
struct SPMCBufferSharedState<T: Clone> {
    /// Data storage buffers
    storage: Vec<Buffer<T>>,

    /// Combination of reader count and latest buffer index (see below)
    latest_buf: AtomicSharedIndex,
}
//
struct Buffer<T: Clone> {
    /// Actual data must be in an UnsafeCell so that Rust knows it's mutable
    data: UnsafeCell<T>,

    /// Amount of readers who are done with this buffer and switched to another
    done_readers: AtomicRefcount,
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
            done_readers: AtomicRefcount::new(
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
///   accidentally end up in a data race w/ the writer. This follows the
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
/// but we nevertheless account for it by adding an overflow bit, whose value is
/// checked in debug builds. A thread which detects such overflow should panic.
///
/// TODO: Switch to U16 / AtomicU16 once the later is stable
///
type BufferIndex = usize;
const INVALID_INDEX: usize = 0xffff;
//
type Refcount = usize;
const UNKNOWN_REFCOUNT: usize = 0xffff;
//
type AtomicRefcount = AtomicUsize;
//
type AtomicSharedIndex = AtomicUsize;
const SHARED_READCOUNT_MASK:   usize = 0b0000_0001_1111_1111; // < 512 readers
const SHARED_OVERFLOW_BIT:     usize = 0b0000_0010_0000_0000;
const SHARED_INDEX_MASK:       usize = 0b1111_1100_0000_0000; // <= 64 buffers
const SHARED_INDEX_MULTIPLIER: usize = 0b0000_0100_0000_0000;


// TODO: Add tests
#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
    }
}
