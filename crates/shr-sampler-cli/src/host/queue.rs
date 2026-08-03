use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Number of events which fit in the MIDI-to-audio handoff.
pub const EVENT_QUEUE_CAPACITY: usize = 1024;

struct Shared<T: Copy> {
    slots: [UnsafeCell<MaybeUninit<T>>; EVENT_QUEUE_CAPACITY],
    read: AtomicUsize,
    write: AtomicUsize,
    overflow: AtomicU64,
    recovery_needed: AtomicBool,
}

// The channel is single-producer/single-consumer. Each side exclusively owns
// its index and accesses a slot only after the matching acquire/release edge.
unsafe impl<T: Copy + Send> Send for Shared<T> {}
unsafe impl<T: Copy + Send> Sync for Shared<T> {}

pub struct Producer<T: Copy>(Arc<Shared<T>>);
pub struct Consumer<T: Copy>(Arc<Shared<T>>);

pub fn channel<T: Copy>() -> (Producer<T>, Consumer<T>) {
    let shared = Arc::new(Shared {
        slots: std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
        read: AtomicUsize::new(0),
        write: AtomicUsize::new(0),
        overflow: AtomicU64::new(0),
        recovery_needed: AtomicBool::new(false),
    });
    (Producer(shared.clone()), Consumer(shared))
}

impl<T: Copy> Producer<T> {
    /// Pushes one event, or drops this newest event when the queue is full.
    pub fn push(&self, value: T) -> bool {
        let write = self.0.write.load(Ordering::Relaxed);
        let read = self.0.read.load(Ordering::Acquire);
        if write.wrapping_sub(read) >= EVENT_QUEUE_CAPACITY {
            self.0.overflow.fetch_add(1, Ordering::Relaxed);
            self.0.recovery_needed.store(true, Ordering::Release);
            return false;
        }
        let index = write % EVENT_QUEUE_CAPACITY;
        // SAFETY: this SPSC producer is the only writer and the capacity check
        // proves that the consumer no longer owns this slot.
        unsafe { (*self.0.slots[index].get()).write(value) };
        self.0.write.store(write.wrapping_add(1), Ordering::Release);
        true
    }

    pub fn overflow_count(&self) -> u64 {
        self.0.overflow.load(Ordering::Relaxed)
    }
}

impl<T: Copy> Consumer<T> {
    pub fn pop(&self) -> Option<T> {
        let read = self.0.read.load(Ordering::Relaxed);
        if read == self.0.write.load(Ordering::Acquire) {
            return None;
        }
        let index = read % EVENT_QUEUE_CAPACITY;
        // SAFETY: acquire observed the producer's initialized slot and this
        // SPSC consumer is its only reader.
        let value = unsafe { (*self.0.slots[index].get()).assume_init_read() };
        self.0.read.store(read.wrapping_add(1), Ordering::Release);
        Some(value)
    }

    pub fn is_empty(&self) -> bool {
        self.0.read.load(Ordering::Relaxed) == self.0.write.load(Ordering::Acquire)
    }

    /// Atomically forgets the current backlog. Used after overflow before the
    /// engine performs All Notes Off, so a dropped note-off cannot hang.
    pub fn discard_all(&self) {
        let write = self.0.write.load(Ordering::Acquire);
        self.0.read.store(write, Ordering::Release);
    }

    pub fn take_overflow_recovery(&self) -> bool {
        self.0.recovery_needed.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_uses_the_documented_capacity_and_preserves_order() {
        let (producer, consumer) = channel();
        for value in 0..EVENT_QUEUE_CAPACITY {
            assert!(producer.push(value));
        }
        assert!(!producer.push(99_999));
        assert_eq!(producer.overflow_count(), 1);
        for expected in 0..EVENT_QUEUE_CAPACITY {
            assert_eq!(consumer.pop(), Some(expected));
        }
        assert_eq!(consumer.pop(), None);
    }

    #[test]
    fn overflow_is_drop_newest_counted_and_recovers_after_panic_flush() {
        let (producer, consumer) = channel();
        for value in 0..EVENT_QUEUE_CAPACITY {
            assert!(producer.push(value));
        }
        assert!(!producer.push(7_777));
        assert!(consumer.take_overflow_recovery());
        consumer.discard_all();
        assert!(!consumer.take_overflow_recovery());
        assert!(producer.push(42));
        assert_eq!(consumer.pop(), Some(42));
        assert_eq!(consumer.pop(), None);
    }
}
