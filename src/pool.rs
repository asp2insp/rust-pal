use std::{mem, fmt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::ops::{Index, IndexMut};
use std::marker::PhantomData;
use std::collections::LinkedList;
use std::ops::{Deref, DerefMut};
use std::ptr;


pub mod tests;

/// Arc is the only valid way to access an item in
/// the pool. It is returned by alloc, and will automatically
/// release/retain when dropped/cloned. It implements Deref/DerefMut,
/// so all accesses can go through it.
/// WARNING! Taking the address of the dereferenced value constitutes
/// undefined behavior. So, given a: Arc<T>, &*a is not allowed
pub struct Arc<T> {
    pool: *mut Pool<T>,
    index: usize,
}

/// Public functions
impl <T> Arc<T> {
    /// If you want to manually manage the memory or
    /// use the wrapped reference outside of the Arc system
    /// the retain/release functions provide an escape hatch.
    /// Retain will increment the reference count
    pub unsafe fn retain(&mut self) {
        self.get_pool().retain(self.index);
    }

    /// If you want to manually manage the memory or
    /// use the wrapped reference outside of the Arc system
    /// the retain/release functions provide an escape hatch.
    /// Release will decrement the reference count
    pub unsafe fn release(&mut self) {
        self.get_pool().release(self.index);
    }
}

/// Internal functions
impl <T> Arc<T> {

    /// It's somewhat confusing that Arc::new()
    /// does not take care of bumping the ref count.
    /// However, the atomic op for claiming a free slot
    /// needs to happen before the new() takes place
    fn new(index: usize, p: &Pool<T>) -> Arc<T> {
        Arc {
            pool: unsafe { mem::transmute(p) },
            index: index,
        }
    }

    fn get_pool(&self) -> &mut Pool<T> {
        unsafe {
            &mut *self.pool
        }
    }

    fn ref_count(&self) -> usize {
        self.get_pool().header_for(self.index).ref_count.load(Ordering::Relaxed)
    }
}

impl <T> Drop for Arc<T> {
    fn drop(&mut self) {
        self.get_pool().release(self.index);
    }
}

impl <T> Clone for Arc<T> {
    fn clone(&self) -> Self {
        self.get_pool().retain(self.index);
        Arc {
            pool: self.pool,
            index: self.index,
        }
    }
}

impl<T> Deref for Arc<T> {
    type Target = T;

    fn deref<'b>(&'b self) -> &'b T {
        &self.get_pool()[self.index]
    }
}

impl<T> DerefMut for Arc<T> {
    fn deref_mut<'b>(&'b mut self) -> &'b mut T {
        &mut self.get_pool()[self.index]
    }
}

impl <T> fmt::Debug for Arc<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Arc{{ offset: {:?}, ref_count: {:?} }}", self.index, self.ref_count())
    }
}

impl <T> PartialEq for Arc<T> {
    fn eq(&self, other: &Arc<T>) -> bool {
        if self.index != other.index {
            false
        } else {
            unsafe {
                self.pool as *const _ == other.pool as *const _
            }
        }
    }
}

/// A pool represents a fixed number of ref-counted objects.
/// The pool treats all given space as an unallocated
/// pool of objects. Each object is prefixed with a header.
/// The header is formatted as follows:
/// * V1
///   - [0..2] ref_count: u16
///
pub struct Pool<T> {
    item_type: PhantomData<T>,

    buffer: *mut u8,
    buffer_size: usize,
    capacity: usize,

    tail: AtomicUsize, // One past the end index

    // Cached values
    slot_size: usize,
    header_size: usize,

    free_list: LinkedList<usize>,
}

struct SlotHeader {
    ref_count: AtomicUsize,
}

/// Public interface
impl <T> Pool<T> {
    pub fn new(mem: &mut [u8]) -> Pool<T> {
        let ptr: *mut u8 = mem.as_mut_ptr();
        let header_size = mem::size_of::<SlotHeader>();
        let slot_size = mem::size_of::<T>() + header_size;
        Pool {
            item_type: PhantomData,
            buffer: ptr,
            buffer_size: mem.len(),
            tail: AtomicUsize::new(0),
            slot_size: slot_size,
            capacity: mem.len() / slot_size,
            header_size: header_size,
            free_list: LinkedList::new(),
        }
    }

    /// Remove all objects from the pool
    /// and zero the memory
    pub unsafe fn clear(&mut self) {
        let mut i = self.buffer.clone();
        let end = self.buffer.clone().offset(self.buffer_size as isize);
        while i != end {
            *i = 0u8;
            i = i.offset(1);
        }
    }

    /// Fast copy a slot's contents to a new slot and return
    /// a pointer to the new slot
    pub fn alloc_with_contents_of(&mut self, other: &Arc<T>) -> Result<Arc<T>, &'static str> {
        let index = try!(self.claim_free_index());
        unsafe {
            let from = self.raw_contents_for(other.index);
            let to = self.raw_contents_for(index);
            ptr::copy(from, to, mem::size_of::<T>());
        }
        Ok(Arc::new(index, self))
    }

    /// Try to allocate a new item from the pool.
    /// A mutable reference to the item is returned on success
    pub fn alloc(&mut self) -> Result<Arc<T>, &'static str> {
        let index = try!(self.internal_alloc());
        Ok(Arc::new(index, self))
    }

    // Increase the ref count for the cell at the given index
    pub fn retain(&mut self, index: usize) {
        let h = self.header_for(index);
        loop {
            let old = h.ref_count.load(Ordering::Relaxed);
            let swap = h.ref_count
            .compare_and_swap(old, old+1, Ordering::Relaxed);
            if swap == old {
                break
            }
        }
    }

    // Decrease the ref count for the cell at the given index
    pub fn release(&mut self, index: usize) {
        let mut is_free = false;
        { // Make the borrow checker happy
            let h = self.header_for(index);
            loop {
                let old = h.ref_count.load(Ordering::Relaxed);
                assert!(old > 0, "Release called on [{}] which has no refs!", index);

                let swap = h.ref_count
                .compare_and_swap(old, old-1, Ordering::Relaxed);
                if swap == old {
                    if old == 1 { // this was the last reference
                        is_free = true;
                    }
                    break
                }
            }
        }
        if is_free {
            self.free_list.push_back(index);
        }
    }

    /// Returns the number of live items. O(1) running time.
    pub fn live_count(&self) -> usize {
        self.tail.load(Ordering::Relaxed) - self.free_list.len()
    }
}


/// Internal Functions
impl <T> Pool<T> {
    // Returns an item from the free list, or
    // tries to allocate a new one from the buffer
    fn claim_free_index(&mut self) -> Result<usize, &'static str> {
        let index = match self.free_list.pop_front() {
            Some(i) => i,
            None => try!(self.push_back_alloc()),
        };
        self.retain(index);
        Ok(index)
    }

    // Internal alloc that does not create an Arc but still claims a slot
    fn internal_alloc(&mut self) -> Result<usize, &'static str> {
        let index = try!(self.claim_free_index());
        Ok(index)
    }

    // Pushes the end of the used space in the buffer back
    // returns the previous index
    fn push_back_alloc(&mut self) -> Result<usize, &'static str> {
        loop {
            let old_tail = self.tail.load(Ordering::Relaxed);
            let swap = self.tail.compare_and_swap(old_tail, old_tail+1, Ordering::Relaxed);
            // If we were the ones to claim this slot, or
            // we've overrun the buffer, return
            if old_tail >= self.capacity {
                return Err("OOM")
            } else if swap == old_tail {
                return Ok(old_tail)
            }
        }
    }

    fn header_for<'a>(&'a mut self, i: usize) -> &'a mut SlotHeader {
        unsafe {
            let ptr = self.buffer.clone()
                .offset((i * self.slot_size) as isize);
            mem::transmute(ptr)
        }
    }

    fn raw_contents_for<'a>(&'a mut self, i: usize) -> *mut u8 {
        unsafe {
            self.buffer.clone()
                .offset((i * self.slot_size) as isize)
                .offset(self.header_size as isize)
        }
    }
}

impl <T> Index<usize> for Pool<T> {
    type Output = T;

    fn index<'a>(&'a self, i: usize) -> &'a T {
        unsafe {
            let ptr = self.buffer.clone()
                .offset((i * self.slot_size) as isize)
                .offset(self.header_size as isize);
            mem::transmute(ptr)
        }
    }
}

impl <T> IndexMut<usize> for Pool<T> {
    fn index_mut<'a>(&'a mut self, i: usize) -> &'a mut T {
        unsafe {
            let ptr = self.buffer.clone()
                .offset((i * self.slot_size) as isize)
                .offset(self.header_size as isize);
            mem::transmute(ptr)
        }
    }
}
