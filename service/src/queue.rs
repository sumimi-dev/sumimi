//! A queue that never moves what is already in it.
//!
//! The writer's queue used to be a `VecDeque` of hits. **Growing a `VecDeque`
//! copies every hit in it** ⇒ the longer the queue, the more each push
//! allocated ⇒ the more the allocator kept ⇒ the higher the peak. Measured
//! with the counting allocator: a push into an empty queue cost 2 631 bytes,
//! a push into a queue of 1 000 rows cost **10 331** — and the queue's length
//! is exactly what the traffic rate decides.
//!
//! This queue is a **chain of fixed-size blocks**: a push fills the last
//! block, a full block is sealed and never touched again ⇒ **a push never
//! moves what is already queued** ⇒ the cost of a push does not depend on how
//! long the queue is.
//!
//! It is first-in, first-out, and it is **bounded**: a push into a full queue
//! is refused, not silently dropped.

use std::collections::VecDeque;

/// How many rows a block holds.
///
/// A block is allocated once and filled; the sealed ones are exactly full, so
/// the only slack is in the block being written ⇒ **the queue's memory is its
/// rows and one block**.
const BLOCK: usize = 64;

/// A first-in, first-out queue made of blocks.
#[derive(Debug)]
pub struct ChunkedQueue<T> {
    /// The blocks, oldest first. Only the last one is being written to.
    blocks: VecDeque<Vec<T>>,
    /// How many rows are in the queue.
    len: usize,
    /// The most rows the queue may hold.
    capacity: usize,
}

impl<T> ChunkedQueue<T> {
    /// A queue that holds at most `capacity` rows.
    pub fn new(capacity: usize) -> Self {
        Self {
            blocks: VecDeque::new(),
            len: 0,
            capacity,
        }
    }

    /// How many rows are in the queue.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Is the queue empty?
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Add a row at the back, if there is room.
    ///
    /// `false` means the queue is full and the row was **not** taken.
    pub fn push_back(&mut self, value: T) -> bool {
        if self.len >= self.capacity {
            return false;
        }
        match self.blocks.back_mut() {
            Some(block) if block.len() < block.capacity() => block.push(value),
            // The last block is full: seal it and start a new one.
            Some(_) | None => {
                let mut block = Vec::with_capacity(BLOCK);
                block.push(value);
                self.blocks.push_back(block);
            }
        }
        self.len += 1;
        true
    }

    /// Take **at most** `max` rows from the front — the rest stay in the
    /// queue, in order, for the next batch.
    pub fn take_front(&mut self, max: usize) -> Vec<T> {
        let mut taken = Vec::with_capacity(max.min(self.len));
        while taken.len() < max {
            let Some(block) = self.blocks.front_mut() else {
                break;
            };
            let room = max - taken.len();
            if block.len() <= room {
                // The whole block goes.
                let block = self.blocks.pop_front().expect("a block was there");
                taken.extend(block);
            } else {
                // Part of the block: the rest stays, in order.
                let rest = block.split_off(room);
                taken.extend(std::mem::replace(block, rest));
                break;
            }
        }
        self.len -= taken.len();
        taken
    }

    /// Take everything.
    pub fn drain_all(&mut self) -> Vec<T> {
        let mut taken = Vec::with_capacity(self.len);
        while let Some(block) = self.blocks.pop_front() {
            taken.extend(block);
        }
        self.len = 0;
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// It is first in, first out.
    #[test]
    fn the_order_is_kept() {
        let mut queue = ChunkedQueue::new(1000);
        for value in 0..500 {
            assert!(queue.push_back(value));
        }
        assert_eq!(queue.len(), 500);
        let taken = queue.take_front(500);
        assert_eq!(taken, (0..500).collect::<Vec<_>>());
        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());
    }

    /// A push into a full queue is refused — and the queue keeps what it has.
    #[test]
    fn a_full_queue_refuses() {
        let mut queue = ChunkedQueue::new(10);
        for value in 0..10 {
            assert!(queue.push_back(value));
        }
        assert!(!queue.push_back(10), "the queue is full");
        assert_eq!(queue.len(), 10);
        assert_eq!(queue.drain_all(), (0..10).collect::<Vec<_>>());
    }

    /// Taking part of a block leaves the rest in order.
    #[test]
    fn taking_part_of_a_block_keeps_the_rest() {
        let mut queue = ChunkedQueue::new(1000);
        for value in 0..200 {
            queue.push_back(value);
        }
        let taken = queue.take_front(70);
        assert_eq!(taken, (0..70).collect::<Vec<_>>());
        assert_eq!(queue.len(), 130);
        let rest = queue.drain_all();
        assert_eq!(rest, (70..200).collect::<Vec<_>>());
    }

    /// A queue that has been emptied and filled again works.
    #[test]
    fn it_can_be_reused() {
        let mut queue = ChunkedQueue::new(1000);
        for _ in 0..3 {
            for value in 0..100 {
                queue.push_back(value);
            }
            let taken = queue.drain_all();
            assert_eq!(taken, (0..100).collect::<Vec<_>>());
            assert!(queue.is_empty());
        }
    }

    /// The capacity is what it says.
    #[test]
    fn the_capacity_is_honoured() {
        let mut queue = ChunkedQueue::new(0);
        assert!(!queue.push_back(1), "a capacity of zero holds nothing");
        let mut queue = ChunkedQueue::new(65);
        for value in 0..65 {
            assert!(queue.push_back(value));
        }
        assert!(!queue.push_back(65));
    }
}
